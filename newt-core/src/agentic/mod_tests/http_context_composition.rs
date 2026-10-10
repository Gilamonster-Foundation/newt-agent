//! Actual provider dispatch consumes a queued composition after closed tools.
use super::*;
use crate::agentic::smart_harness::{AdjudicationSettings, SmartHarness};
use serde_json::{json, Value};

fn page_in(value: &Value) -> Option<Value> {
    match value {
        Value::Object(o) if o.contains_key("cards") && o.contains_key("catalog") => {
            Some(value.clone())
        }
        Value::Object(o) => o.values().find_map(page_in),
        Value::Array(a) => a.iter().find_map(page_in),
        Value::String(s) => serde_json::from_str::<Value>(s)
            .ok()
            .and_then(|v| page_in(&v)),
        _ => None,
    }
}

fn response(wire: &str, round: usize, args: Option<Value>) -> Value {
    let id = format!("call_{round}");
    match (wire, args) {
        ("ollama", Some(args)) => {
            json!({"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"propose_context","arguments":args}}]},"done":true})
        }
        ("anthropic", Some(args)) => {
            json!({"id":"msg_1","type":"message","role":"assistant","model":"test-model","stop_reason":"tool_use","content":[{"type":"tool_use","id":id,"name":"propose_context","input":args}]})
        }
        ("responses", Some(args)) => {
            json!({"id":"resp_1","status":"completed","output":[{"type":"function_call","id":format!("fc_{round}"),"call_id":id,"name":"propose_context","arguments":args.to_string()}]})
        }
        (_, Some(args)) => {
            json!({"choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"id":id,"type":"function","function":{"name":"propose_context","arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]})
        }
        ("ollama", None) => {
            json!({"message":{"role":"assistant","content":"The answer is three."},"done":true})
        }
        ("anthropic", None) => {
            json!({"id":"msg_1","type":"message","role":"assistant","model":"test-model","stop_reason":"end_turn","content":[{"type":"text","text":"The answer is three."}],"usage":{"input_tokens":10,"output_tokens":5}})
        }
        ("responses", None) => {
            json!({"id":"resp_1","status":"completed","model":"test-model","output":[{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"The answer is three.","annotations":[]}]}]})
        }
        (_, None) => {
            json!({"choices":[{"message":{"role":"assistant","content":"The answer is three."},"finish_reason":"stop"}]})
        }
    }
}

#[tokio::test]
async fn composition_primary_tool_runs_in_all_four_provider_loops() {
    let _settings = default_loop_settings();
    for wire in ["ollama", "openai", "anthropic", "responses"] {
        let server = MockServer::start().await;
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        let wire_owned = wire.to_owned();
        let old = "OBSOLETE_DETAIL_".repeat(100);
        let old_for_server = old.clone();
        Mock::given(method("POST")).respond_with(move |request:&Request| {
            let n=counter.fetch_add(1,Ordering::SeqCst);
            let body=body_json(request);
            let args=match n {
                0=>Some(json!({})),
                1=>{
                    let page=page_in(&body).expect("real catalog tool response reaches next request");
                    let cid=page["cards"].as_array().unwrap().iter().find(|c| c["preview"].as_str().unwrap().contains("OBSOLETE_DETAIL_")).unwrap()["cid"].clone();
                    Some(json!({"catalog":page["catalog"],"expected_head":page["expected_head"],"changes":[{"cid":cid,"action":"park","reason":"older task detail"}]}))
                },
                _=>{
                    assert!(!String::from_utf8_lossy(&request.body).contains(&old_for_server),"{wire_owned}: parked message reached actual transport");
                    None
                }
            };
            ResponseTemplate::new(200).set_body_json(response(&wire_owned,n,args))
        }).mount(&server).await;
        let h = SmartHarness::new(
            agent_harness::Session::new(crate::test_guard::unbudgeted_session_config()).unwrap(),
            Arc::new(|_| Box::pin(async { Ok(("\"answer\"".into(), None)) })),
            AdjudicationSettings {
                composition_enabled: true,
                ..Default::default()
            },
        )
        .unwrap();
        let m = vec![
            MemMessage::system("you are a test"),
            MemMessage::user(&old),
            MemMessage::assistant("prior answer"),
            MemMessage::user("do the thing"),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut context = ctx(&uri, &m, &caveats);
        context.smart_harness = Some(&h);
        context.max_tool_rounds = 5;
        context.kind = match wire {
            "ollama" => BackendKind::Ollama,
            "anthropic" => BackendKind::Anthropic,
            _ => BackendKind::Openai,
        };
        let result = if wire == "responses" {
            openai_responses_complete(context, &mut NoMcp).await
        } else {
            chat_complete(context, &mut NoMcp).await
        };
        assert!(result.is_ok(), "{wire}: {result:?}");
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3, "{wire}");
        assert_eq!(h.replay_last_request().unwrap(), requests[2].body, "{wire}");
    }
}
