use super::*;

/// Grounds `memory_fetch_with_source_routes_through_execute_tool` against a
/// real directory and session store: the advertised JSON instruction must
/// recover the omitted middle on the next provider request, in a read-only
/// (Explain) turn with minimal exposure.
///
/// The spill comes from an oversized `list_dir`. It used to come from
/// `read_file`, which now pages under the spill cap (`offset=` to continue)
/// and never spills — a large file's middle is reached by paging, not by a
/// handle.
#[tokio::test]
async fn spill_retrieval_round_trip_recovers_the_listing_middle_on_the_wire() {
    let workspace = tempfile::tempdir().unwrap();
    let dir = workspace.path().join("many");
    std::fs::create_dir(&dir).unwrap();
    // ~24k chars of names, over the 16k spill cap; the marker sorts into the
    // middle, far from the 800-char head and tail the teaser keeps.
    let mut names: Vec<String> = (0..400)
        .map(|i| format!("entry_{i:04}_{}", "p".repeat(50)))
        .collect();
    names.push("entry_0200_EXACT_MIDDLE_DETAIL".to_string());
    for name in &names {
        std::fs::write(dir.join(name), b"").unwrap();
    }
    names.sort();
    let exact = names.join("\n");
    let spill = SessionSpillStore::new([7; 16]);
    let source = StoreMemorySource::from_stores(None, None).with_spill_store(&spill);
    let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen = requests.clone();
    let server = MockServer::start().await;
    let replay = DisplayReplay::default();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let body = body_json(request);
            if replay.take(request) {
                return sse_replay("The recovered detail is EXACT_MIDDLE_DETAIL.");
            }
            let mut requests = seen.lock().unwrap();
            let round = requests.len();
            requests.push(body.clone());
            let call = match round {
                0 => Some(("list_dir", serde_json::json!({"path": "many"}))),
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
                None => {
                    replay.arm(request);
                    serde_json::json!({"role": "assistant",
                        "content": "The recovered detail is EXACT_MIDDLE_DETAIL."})
                }
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
