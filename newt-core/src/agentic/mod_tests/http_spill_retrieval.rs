use super::*;

/// Grounds `memory_fetch_with_source_routes_through_execute_tool` against a
/// real file and session store: the advertised JSON instruction must recover
/// the omitted middle on the next provider request, including minimal exposure.
#[tokio::test]
async fn spill_retrieval_round_trip_recovers_file_middle_on_the_wire() {
    let workspace = tempfile::tempdir().unwrap();
    let exact = format!(
        "{}\nEXACT_MIDDLE_DETAIL\n{}",
        "a".repeat(9_000),
        "z".repeat(9_000)
    );
    std::fs::write(workspace.path().join("refs.txt"), &exact).unwrap();
    let spill = SessionSpillStore::new([7; 16]);
    let source = StoreMemorySource::from_stores(None, None).with_spill_store(&spill);
    let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen = requests.clone();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let body = body_json(request);
            if is_stream(request) {
                return sse_replay("The recovered detail is EXACT_MIDDLE_DETAIL.");
            }
            let mut requests = seen.lock().unwrap();
            let round = requests.len();
            requests.push(body.clone());
            let call = match round {
                0 => Some(("read_file", serde_json::json!({"path": "refs.txt"}))),
                1 => {
                    let teaser = body["messages"].as_array().unwrap().last().unwrap()["content"]
                        .as_str()
                        .unwrap();
                    let arguments = teaser
                        .split_once("JSON arguments ")
                        .and_then(|(_, text)| {
                            serde_json::Deserializer::from_str(text)
                                .into_iter::<serde_json::Value>()
                                .next()
                        })
                        .and_then(Result::ok)
                        .unwrap_or_else(|| serde_json::json!({"address": "missing instruction"}));
                    Some(("memory_fetch", arguments))
                }
                _ => None,
            };
            let message = match call {
                Some((name, arguments)) => serde_json::json!({
                    "role": "assistant", "content": null,
                    "tool_calls": [{"id": format!("call_{round}"), "type": "function",
                        "function": {"name": name, "arguments": arguments.to_string()}}]
                }),
                None => serde_json::json!({"role": "assistant",
                    "content": "The recovered detail is EXACT_MIDDLE_DETAIL."}),
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": message}]
            }))
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut context = ctx(&uri, &messages, &caveats);
    context.kind = BackendKind::Openai;
    context.workspace = workspace.path().to_str().unwrap();
    context.prompt_disposition = PromptDisposition::Explain;
    context.tool_offload = true;
    context.spill_store = Some(&spill);
    context.memory_source = Some(&source);
    context.exposure.profile = crate::config::ExposureProfile::Minimal;
    let (reply, _, _, _) = chat_complete(context, &mut NoMcp).await.unwrap();
    assert!(reply.contains("EXACT_MIDDLE_DETAIL"));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for request in requests.iter() {
        assert!(request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "memory_fetch"));
    }
    let tool_text = |request: &serde_json::Value| {
        request["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(!tool_text(&requests[1]).contains("EXACT_MIDDLE_DETAIL"));
    assert_eq!(tool_text(&requests[2]), exact);
    assert_eq!(
        spill.unique_objects(),
        1,
        "retrieval must not create a second spill"
    );
}
