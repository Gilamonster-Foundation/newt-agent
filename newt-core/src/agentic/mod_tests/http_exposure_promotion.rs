use super::*;
use crate::agentic::note_sink::tests::MockSink;

/// The four agentic loops, each with its own wire. Exposure promotion is wired
/// into every loop, so every loop replays the scenario.
#[derive(Clone, Copy, Debug)]
enum Wire {
    Ollama,
    ChatCompletions,
    Responses,
    Anthropic,
}

impl Wire {
    fn path(self) -> &'static str {
        match self {
            Self::Ollama => "/api/chat",
            Self::ChatCompletions => "/v1/chat/completions",
            Self::Responses => "/v1/responses",
            Self::Anthropic => "/v1/messages",
        }
    }

    /// The tool names a provider request advertised, in wire order.
    fn tool_names(self, body: &serde_json::Value) -> Vec<String> {
        let name = |tool: &serde_json::Value| {
            match self {
                Self::Ollama | Self::ChatCompletions => tool["function"]["name"].as_str(),
                Self::Responses | Self::Anthropic => tool["name"].as_str(),
            }
            .map(String::from)
        };
        body["tools"]
            .as_array()
            .map(|tools| tools.iter().filter_map(name).collect())
            .unwrap_or_default()
    }

    /// The text of the newest tool result a provider request carried.
    fn last_tool_result(self, body: &serde_json::Value) -> String {
        let text = |value: &serde_json::Value| match value {
            serde_json::Value::String(text) => text.clone(),
            serde_json::Value::Array(blocks) => blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect(),
            _ => String::new(),
        };
        let items = match self {
            Self::Responses => body["input"].as_array(),
            _ => body["messages"].as_array(),
        };
        items
            .into_iter()
            .flatten()
            .rev()
            .find_map(|item| match self {
                Self::Ollama | Self::ChatCompletions => {
                    (item["role"] == "tool").then(|| text(&item["content"]))
                }
                Self::Responses => {
                    (item["type"] == "function_call_output").then(|| text(&item["output"]))
                }
                Self::Anthropic => item["content"].as_array().and_then(|blocks| {
                    blocks
                        .iter()
                        .rev()
                        .find(|block| block["type"] == "tool_result")
                        .map(|block| text(&block["content"]))
                }),
            })
            .unwrap_or_default()
    }

    /// The provider's reply for one round: a tool call, or the final answer.
    fn reply(self, round: usize, call: Option<&(&str, serde_json::Value)>) -> ResponseTemplate {
        let id = format!("call_{round}");
        let body = match (self, call) {
            (Self::Ollama, Some((name, arguments))) => serde_json::json!({
                "prompt_eval_count": 1, "eval_count": 1,
                "message": {"content": "", "tool_calls": [{"function": {"name": name, "arguments": arguments}}]}
            }),
            (Self::Ollama, None) => serde_json::json!({
                "prompt_eval_count": 1, "eval_count": 1, "message": {"content": "done"}
            }),
            (Self::ChatCompletions, Some((name, arguments))) => serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [
                    {"id": id, "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}
                ]}}]
            }),
            (Self::ChatCompletions, None) => serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "done"}}]
            }),
            (Self::Responses, Some((name, arguments))) => serde_json::json!({
                "status": "completed",
                "output": [{"type": "function_call", "call_id": id, "name": name, "arguments": arguments.to_string()}]
            }),
            (Self::Responses, None) => serde_json::json!({
                "status": "completed",
                "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]}]
            }),
            (Self::Anthropic, Some((name, arguments))) => serde_json::json!({
                "model": "claude-test", "stop_reason": "tool_use",
                "content": [{"type": "tool_use", "id": id, "name": name, "input": arguments}],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }),
            (Self::Anthropic, None) => serde_json::json!({
                "model": "claude-test", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "done"}],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }),
        };
        ResponseTemplate::new(200).set_body_json(body)
    }

    /// Point `context` at this wire and run the turn.
    async fn run(self, mut context: ChatCtx<'_>, mcp: &mut dyn McpTools) {
        match self {
            Self::Ollama => {
                chat_complete(context, mcp).await.unwrap();
            }
            Self::ChatCompletions => {
                context.kind = BackendKind::Openai;
                chat_complete(context, mcp).await.unwrap();
            }
            Self::Responses => {
                context.kind = BackendKind::Openai;
                openai_responses_complete(context, mcp).await.unwrap();
            }
            Self::Anthropic => {
                context.kind = BackendKind::Anthropic;
                context.api_key = Some("sk-ant-test");
                chat_complete(context, mcp).await.unwrap();
            }
        }
    }
}

/// A backend on `wire` that plays one scripted tool call per model round, then
/// answers, recording every model-round request body it received.
async fn scripted(
    wire: Wire,
    calls: Vec<(&'static str, serde_json::Value)>,
) -> (MockServer, Arc<Mutex<Vec<serde_json::Value>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let replay = DisplayReplay::default();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(wire.path()))
        .respond_with(move |request: &Request| {
            let body = body_json(request);
            // A display re-issue of the final answer is not a model round:
            // Chat Completions repeats the final request verbatim, the other
            // wires re-issue it streamed.
            match wire {
                Wire::ChatCompletions if replay.take(request) => return sse_replay("done"),
                Wire::ChatCompletions => {}
                _ if body["stream"] == true => return wire.reply(usize::MAX, None),
                _ => {}
            }
            let mut requests = seen.lock().unwrap();
            requests.push(body);
            let round = requests.len() - 1;
            if calls.get(round).is_none() {
                replay.arm(request);
            }
            wire.reply(round, calls.get(round))
        })
        .mount(&server)
        .await;
    (server, requests)
}

/// Generate one test per wire for a scenario. Anthropic runs with its JSON
/// reply shape, serialized with the Anthropic loop tests that own that switch.
macro_rules! per_wire {
    ($scenario:ident: $ollama:ident, $chat:ident, $responses:ident, $anthropic:ident) => {
        #[tokio::test]
        async fn $ollama() {
            $scenario(Wire::Ollama).await;
        }
        #[tokio::test]
        async fn $chat() {
            $scenario(Wire::ChatCompletions).await;
        }
        #[tokio::test]
        async fn $responses() {
            $scenario(Wire::Responses).await;
        }
        #[tokio::test]
        #[serial_test::serial(anthropic_loop_env)]
        async fn $anthropic() {
            let _env = crate::agentic::anthropic_loop_tests::test_env(false);
            $scenario(Wire::Anthropic).await;
        }
    };
}

/// #2331 child 1: an authorized tool the exposure controller leaves off the
/// wire is discoverable and becomes callable without a mode toggle.
///
/// Under `minimal` exposure `save_note` (ByIntent) is authorized but unexposed.
/// `tool_search` must name it as authorized with its schema not loaded, not as
/// an ordinary callable match. The first call must NOT run (no arguments run
/// against a schema the model never saw); it promotes the tool, and the next
/// request APPENDS its schema after the tools already sent, so the provider's
/// cached prefix changes as little as possible. The second call runs.
async fn an_exposure_hidden_tool_is_found_then_promoted(wire: Wire) {
    let note = serde_json::json!({"action": "add", "text": "promoted note"});
    let (server, requests) = scripted(
        wire,
        vec![
            ("tool_search", serde_json::json!({"query": "save note"})),
            ("save_note", note.clone()),
            ("save_note", note),
        ],
    )
    .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut sink = MockSink::default();
    let mut context = ctx(&uri, &messages, &caveats);
    context.action_nudges = false;
    context.note_sink = Some(&mut sink);
    context.exposure.profile = crate::config::ExposureProfile::Minimal;
    wire.run(context, &mut NoMcp).await;

    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        4,
        "{wire:?}: search, hidden call, promoted call, answer"
    );
    let names: Vec<Vec<String>> = requests.iter().map(|r| wire.tool_names(r)).collect();
    assert!(
        !names[0].contains(&"save_note".to_string()),
        "{wire:?}: {:?}",
        names[0]
    );

    let search = wire.last_tool_result(&requests[1]);
    let line = search
        .lines()
        .find(|line| line.starts_with("- save_note"))
        .unwrap_or_default();
    assert!(
        line.contains("schema not loaded"),
        "{wire:?}: an authorized unexposed tool must be found and marked, not offered as loaded: {search}"
    );
    assert_eq!(names[1], names[0], "{wire:?}: a search promotes nothing");

    let coached = wire.last_tool_result(&requests[2]);
    assert!(coached.contains("schema not loaded"), "{wire:?}: {coached}");
    let mut expected = names[0].clone();
    expected.push("save_note".to_string());
    assert_eq!(
        names[2], expected,
        "{wire:?}: the promoted schema is appended after the tools already sent"
    );
    assert_eq!(
        names[3], expected,
        "{wire:?}: promotion is sticky for the turn"
    );
    drop(requests);
    assert_eq!(
        sink.calls,
        vec!["add:promoted note".to_string()],
        "{wire:?}: only the call made against the loaded schema runs"
    );
}

per_wire!(an_exposure_hidden_tool_is_found_then_promoted:
    an_exposure_hidden_tool_is_found_then_promoted_for_the_next_request,
    chat_completions_promotes_an_exposure_hidden_tool,
    responses_promotes_an_exposure_hidden_tool,
    anthropic_promotes_an_exposure_hidden_tool);

/// Twin: the default `full` profile is bit-identical. Every request carries the
/// same tool list, nothing is marked unloaded, and the first call runs.
async fn full_exposure_sends_one_tool_list(wire: Wire) {
    let (server, requests) = scripted(
        wire,
        vec![
            ("tool_search", serde_json::json!({"query": "save note"})),
            (
                "save_note",
                serde_json::json!({"action": "add", "text": "note"}),
            ),
        ],
    )
    .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut sink = MockSink::default();
    let mut context = ctx(&uri, &messages, &caveats);
    context.action_nudges = false;
    context.note_sink = Some(&mut sink);
    wire.run(context, &mut NoMcp).await;

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "{wire:?}");
    let first = wire.tool_names(&requests[0]);
    assert!(
        first.contains(&"save_note".to_string()),
        "{wire:?}: {first:?}"
    );
    for request in requests.iter() {
        assert_eq!(wire.tool_names(request), first, "{wire:?}");
        assert!(!wire.last_tool_result(request).contains("schema not loaded"));
    }
    drop(requests);
    assert_eq!(sink.calls, vec!["add:note".to_string()], "{wire:?}");
}

per_wire!(full_exposure_sends_one_tool_list:
    full_exposure_sends_one_tool_list_and_runs_the_first_call,
    chat_completions_full_exposure_is_unchanged,
    responses_full_exposure_is_unchanged,
    anthropic_full_exposure_is_unchanged);

/// A generic MCP tool the session connected but this request's disposition
/// refuses.
struct RefusedRemote {
    calls: usize,
}

#[async_trait::async_trait]
impl McpTools for RefusedRemote {
    fn handles(&self, name: &str) -> bool {
        name == "review__fetch"
    }
    fn tool_defs(&self) -> Vec<serde_json::Value> {
        vec![serde_json::json!({
            "type": "function",
            "function": {"name": "review__fetch", "description": "fetch a review", "parameters": {}}
        })]
    }
    async fn call(&mut self, _leased: &LeasedMcpCall<'_>) -> String {
        self.calls += 1;
        "fetched".to_string()
    }
}

/// Twin: a capability the request refuses is never `KnownHidden`. Under
/// Explain with `minimal` exposure, the refused MCP tool is not marked
/// unloaded by search, its call is refused rather than promoted, and it never
/// reaches the wire.
#[tokio::test]
async fn a_refused_capability_is_never_marked_hidden_or_promoted() {
    let wire = Wire::Ollama;
    let (server, requests) = scripted(
        wire,
        vec![
            ("tool_search", serde_json::json!({"query": "review fetch"})),
            ("review__fetch", serde_json::json!({})),
        ],
    )
    .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut context = ctx(&uri, &messages, &caveats);
    context.action_nudges = false;
    context.prompt_disposition = PromptDisposition::Explain;
    context.exposure.profile = crate::config::ExposureProfile::Minimal;
    let mut mcp = RefusedRemote { calls: 0 };
    wire.run(context, &mut mcp).await;

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(!wire
        .last_tool_result(&requests[1])
        .contains("schema not loaded"));
    let refused = wire.last_tool_result(&requests[2]);
    assert!(
        refused.contains("Tool `review__fetch` is not available for this request"),
        "{refused}"
    );
    assert!(!refused.contains("schema not loaded"), "{refused}");
    for request in requests.iter() {
        assert!(!wire
            .tool_names(request)
            .contains(&"review__fetch".to_string()));
    }
    drop(requests);
    assert_eq!(mcp.calls, 0);
}
