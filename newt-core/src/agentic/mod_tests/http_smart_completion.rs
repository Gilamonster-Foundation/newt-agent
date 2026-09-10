//! Completed calls must survive an interrupted sibling in the same provider batch.
use super::*;
use crate::launch_authority::{self, LaunchAuthority};

const TOOL: &str = "durability__get_result";
const COMPLETED: &str = "first call completed: durable payload";

/// Match production startup on the current-thread test runtime: authority
/// stays frozen while unrelated environment-driven tests exercise their flags.
struct ConfinedLaunch;

impl ConfinedLaunch {
    fn new() -> Self {
        launch_authority::freeze(LaunchAuthority::CONFINED);
        assert_eq!(launch_authority::current(), LaunchAuthority::CONFINED);
        Self
    }
}

impl Drop for ConfinedLaunch {
    fn drop(&mut self) {
        launch_authority::reset_for_test();
    }
}

struct InterruptedBatch {
    started: Arc<tokio::sync::Notify>,
    calls: Vec<String>,
    effect: std::path::PathBuf,
    first: String,
    effect_text: String,
}

#[async_trait::async_trait]
impl McpTools for InterruptedBatch {
    fn handles(&self, name: &str) -> bool {
        name == TOOL
    }

    fn tool_defs(&self) -> Vec<serde_json::Value> {
        vec![serde_json::json!({"type":"function","function":{
            "name":TOOL,"description":"Read the requested fixture result.",
            "parameters":{"type":"object","properties":{"stage":{"type":"string"}},"required":["stage"]}
        }})]
    }

    async fn call(&mut self, leased: &LeasedMcpCall<'_>) -> String {
        let stage = leased.args()["stage"].as_str().unwrap();
        self.calls.push(stage.to_string());
        if stage == "a" {
            std::fs::write(&self.effect, &self.effect_text).unwrap();
            return self.first.clone();
        }
        assert_eq!(stage, "b");
        self.started.notify_one();
        std::future::pending().await
    }
}

fn batch_reply(wire: &str) -> serde_json::Value {
    let calls = ["a", "b", "c"]
        .into_iter()
        .map(|stage| {
            let id = format!("call_{stage}");
            let args = serde_json::json!({"stage":stage});
            match wire {
                "ollama" => serde_json::json!({"function":{"name":TOOL,"arguments":args}}),
                "anthropic" | "openai_native" => serde_json::json!({"type":"tool_use","id":id,"name":TOOL,"input":args}),
                "responses" => serde_json::json!({"type":"function_call","id":format!("fc_{stage}"),"call_id":id,"name":TOOL,"arguments":args.to_string()}),
                _ => serde_json::json!({"type":"function","id":id,"function":{"name":TOOL,"arguments":args.to_string()}}),
            }
        })
        .collect::<Vec<_>>();
    match wire {
        "ollama" => {
            serde_json::json!({"message":{"role":"assistant","content":"","tool_calls":calls},"done":true})
        }
        "anthropic" => {
            serde_json::json!({"id":"msg_batch","type":"message","role":"assistant","model":"test-model","stop_reason":"tool_use","content":calls})
        }
        "responses" => {
            let mut output =
                vec![serde_json::json!({"type":"reasoning","id":"reason_batch","summary":[]})];
            output.extend(calls);
            serde_json::json!({"id":"resp_batch","status":"completed","output":output})
        }
        _ => {
            serde_json::json!({"choices":[{"message":{"role":"assistant","content":"","tool_calls":calls},"finish_reason":"tool_calls"}]})
        }
    }
}

async fn completed_call_survives_cancelled_sibling(wire: &str) {
    recovered_after_sibling(wire, false, false).await;
}

async fn recovered_after_sibling(wire: &str, abrupt: bool, spill: bool) {
    let _launch = ConfinedLaunch::new();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(batch_reply(wire)))
        .expect(1)
        .mount(&server)
        .await;
    let workspace = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let config = agent_harness::SessionConfig::default();
    let harness = SmartHarness::new(
        agent_harness::Session::open(directory.path(), config.clone()).unwrap(),
        Arc::new(|_| panic!("an interrupted tool batch must not invoke adjudication")),
        AdjudicationSettings::default(),
    )
    .unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let full = if spill {
        "real retained large file effect\n".repeat(1000)
    } else {
        COMPLETED.into()
    };
    let first = if spill {
        let (handle, _) = crate::agentic::content_spill::store_redacted_full(
            &full,
            Some(TOOL.into()),
            harness.spill_store(),
        );
        format!(
            "{COMPLETED}\n{}",
            crate::agentic::content_spill::tool_output_retrieval_hint(&handle.unwrap())
        )
    } else {
        COMPLETED.into()
    };
    let mut mcp = InterruptedBatch {
        started: started.clone(),
        calls: Vec::new(),
        effect: workspace.path().join("completed-a.txt"),
        first,
        effect_text: full.clone(),
    };
    let cancel = AtomicBool::new(false);
    let mut reason = None;
    let caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
    let allow = vec![TOOL.to_string()];
    let uri = server.uri();
    let current = msgs();
    let mut context = ctx(&uri, &current, &caveats);
    context.workspace = workspace.path().to_str().unwrap();
    context.smart_harness = Some(&harness);
    context.persona_tools = Some(&allow);
    context.cancel = Some(&cancel);
    context.end_reason = Some(&mut reason);
    context.kind = match wire {
        "ollama" => BackendKind::Ollama,
        "anthropic" => BackendKind::Anthropic,
        _ => BackendKind::Openai,
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let run = async {
            if wire == "responses" {
                openai_responses_complete(context, &mut mcp).await
            } else {
                chat_complete(context, &mut mcp).await
            }
        };
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => panic!("{wire}: loop stopped before the second call blocked: {result:?}"),
            _ = started.notified() => {}
        }
        // The second dispatch proves that A returned and its normal completion
        // bookkeeping ran. Cancellation cannot win before that boundary.
        if abrupt { return None; }
        cancel.store(true, Ordering::SeqCst);
        Some(run.await)
    })
    .await
    .expect("the deterministic second-call barrier must be reached");
    if let Some(result) = result {
        assert!(result
            .expect("cancellation is a reported terminal outcome")
            .0
            .is_empty());
        assert_eq!(reason, Some(crate::TurnEndReason::Cancelled));
    } else {
        assert!(
            reason.is_none(),
            "dropping the caller cannot report a terminal outcome"
        );
    }
    assert_eq!(mcp.calls, ["a", "b"]);
    assert_eq!(std::fs::read_to_string(&mcp.effect).unwrap(), full);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    let head = harness.head().unwrap();
    // Dropping the harness also destroys its authoritative live spill store.
    drop(harness);

    let mut restored = agent_harness::Session::restore(directory.path(), head, &config.authority)
        .expect("fresh storage restore after dropping the runtime");
    let messages = restored.restored_messages().unwrap();
    let completed = messages
        .iter()
        .filter(|message| content(message).is_some_and(|text| text.contains(COMPLETED)))
        .collect::<Vec<_>>();
    assert_eq!(
        completed.len(),
        1,
        "{wire}: restore retains actual A exactly once: {messages:?}"
    );
    let ids = invocation_ids(directory.path(), head);
    assert_eq!(ids.len(), 3);
    for (index, expected) in [
        agent_harness::ToolCallState::Returned,
        agent_harness::ToolCallState::Uncertain,
        agent_harness::ToolCallState::NotStarted,
    ]
    .into_iter()
    .enumerate()
    {
        let status = restored.tool_call(ids[index]).unwrap();
        assert_eq!(status.state, expected, "{wire} slot {index}");
        assert_eq!(status.returned.is_some(), index == 0);
        let store = agent_harness::store::FrameStore::open(directory.path()).unwrap();
        let event = agent_harness::forensics::inspect_from_store(
            &store,
            status.delivery.unwrap(),
            Default::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            event.record["origin"],
            if index == 0 && !spill {
                "tool"
            } else {
                "harness"
            }
        );
    }
    if wire != "ollama" {
        assert_eq!(
            completed[0]
                .get("tool_call_id")
                .or_else(|| completed[0].get("call_id"))
                .and_then(serde_json::Value::as_str),
            Some("call_a")
        );
    }
    if spill {
        let sources = restored.tool_call(ids[0]).unwrap().retained_sources.clone();
        assert_eq!(sources.len(), 1);
        let mut recovered = String::new();
        let mut offset = 0;
        loop {
            let slice = restored
                .re_read(&sources[0].to_string(), offset, 4096)
                .unwrap();
            recovered.push_str(slice["text"].as_str().unwrap());
            if slice["complete"] == true {
                break;
            }
            let next = slice["next_offset"].as_u64().unwrap() as usize;
            assert!(next > offset);
            offset = next;
        }
        assert_eq!(recovered, full);
    }
    // A cold runtime must send legal call/output closure without re-executing
    // the ambiguous B side effect or queued C. The transport sees the proof.
    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer_reply(wire)))
        .expect(1)
        .mount(&server)
        .await;
    let harness = SmartHarness::new(
        restored,
        Arc::new(|_| Box::pin(async { Ok("\"answer\"".into()) })),
        Default::default(),
    )
    .unwrap();
    let current = msgs();
    let mut context = ctx(&uri, &current, &caveats);
    context.workspace = workspace.path().to_str().unwrap();
    context.smart_harness = Some(&harness);
    context.persona_tools = Some(&allow);
    context.kind = backend(wire);
    let resumed = if wire == "responses" {
        openai_responses_complete(context, &mut mcp).await
    } else {
        chat_complete(context, &mut mcp).await
    }
    .unwrap();
    assert_eq!(resumed.0, "Recovery reviewed.");
    assert_eq!(
        mcp.calls,
        ["a", "b"],
        "restore must never schedule old calls"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(harness.replay_last_request().unwrap(), requests[0].body);
    let body = body_json(&requests[0]);
    assert!(String::from_utf8_lossy(&requests[0].body).contains(COMPLETED));
    assert_request_closure(&body, wire);
}

/// Grounds the provider-loop completion contract in a fresh disk restore after
/// the MCP fixture writes a real file for A and deterministically blocks B.
#[tokio::test]
#[serial_test::serial]
async fn ollama_completed_call_survives_cancelled_sibling() {
    completed_call_survives_cancelled_sibling("ollama").await;
}

/// Grounds OpenAI's parallel-call reply shape in the same real-storage boundary.
#[tokio::test]
#[serial_test::serial]
async fn openai_completed_call_survives_cancelled_sibling() {
    completed_call_survives_cancelled_sibling("openai").await;
}

/// Grounds Anthropic's multiple tool-use blocks in the real-storage boundary.
#[tokio::test]
#[serial_test::serial]
async fn anthropic_completed_call_survives_cancelled_sibling() {
    completed_call_survives_cancelled_sibling("anthropic").await;
}

/// Grounds Responses call/output identity retention in the real-storage boundary.
#[tokio::test]
#[serial_test::serial]
async fn responses_completed_call_survives_cancelled_sibling() {
    completed_call_survives_cancelled_sibling("responses").await;
}

fn content(message: &serde_json::Value) -> Option<&str> {
    message
        .get("content")
        .or_else(|| message.get("output"))
        .and_then(serde_json::Value::as_str)
}

fn backend(wire: &str) -> BackendKind {
    match wire {
        "ollama" => BackendKind::Ollama,
        "anthropic" => BackendKind::Anthropic,
        _ => BackendKind::Openai,
    }
}

fn answer_reply(wire: &str) -> serde_json::Value {
    let text = "Recovery reviewed.";
    match wire {
        "ollama" => serde_json::json!({"message":{"role":"assistant","content":text},"done":true}),
        "anthropic" => {
            serde_json::json!({"id":"msg_answer","type":"message","role":"assistant","model":"test-model","stop_reason":"end_turn","content":[{"type":"text","text":text}]})
        }
        "responses" => {
            serde_json::json!({"id":"resp_answer","status":"completed","output":[{"type":"message","id":"msg_answer","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]}]})
        }
        _ => {
            serde_json::json!({"choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}]})
        }
    }
}

fn invocation_ids(
    directory: &std::path::Path,
    mut head: content_addressable::ContentId,
) -> Vec<content_addressable::ContentId> {
    let store = agent_harness::store::FrameStore::open(directory).unwrap();
    loop {
        let record = agent_harness::forensics::inspect_from_store(&store, head, Default::default())
            .unwrap()
            .unwrap();
        let ids = record
            .references
            .iter()
            .filter(|reference| reference.relation == "invocation")
            .map(|reference| reference.cid.parse().unwrap())
            .collect::<Vec<_>>();
        // ToolCall records refer to one invocation; ToolBatch carries all three.
        if ids.len() > 1 {
            return ids;
        }
        head = *record
            .parents
            .first()
            .expect("a batch must be in the journal ancestry");
    }
}

// Restore permissions even if an assertion or tool future unwinds.
struct CheckpointPermissions {
    path: std::path::PathBuf,
    original: std::fs::Permissions,
}
impl CheckpointPermissions {
    fn read_only(path: &std::path::Path) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let original = std::fs::metadata(path).unwrap().permissions();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o555)).unwrap();
        Self {
            path: path.to_path_buf(),
            original,
        }
    }
}
impl Drop for CheckpointPermissions {
    fn drop(&mut self) {
        std::fs::set_permissions(&self.path, self.original.clone())
            .expect("restore fixture permissions");
    }
}

struct ReturningBatch {
    calls: Vec<String>,
    outputs: Vec<String>,
    effect: std::path::PathBuf,
    fail_checkpoint: Option<std::path::PathBuf>,
    last_checkpoint: Option<String>,
    permissions: Option<CheckpointPermissions>,
}

#[async_trait::async_trait]
impl McpTools for ReturningBatch {
    fn handles(&self, name: &str) -> bool {
        name == TOOL
    }
    fn tool_defs(&self) -> Vec<serde_json::Value> {
        InterruptedBatch {
            started: Arc::new(tokio::sync::Notify::new()),
            calls: Vec::new(),
            effect: self.effect.clone(),
            first: String::new(),
            effect_text: String::new(),
        }
        .tool_defs()
    }
    async fn call(&mut self, leased: &LeasedMcpCall<'_>) -> String {
        let stage = leased.args()["stage"].as_str().unwrap().to_owned();
        self.calls.push(stage.clone());
        if stage == "a" {
            std::fs::write(&self.effect, &self.outputs[0]).unwrap();
            if let Some(path) = &self.fail_checkpoint {
                self.last_checkpoint = Some(std::fs::read_to_string(path).unwrap());
                self.permissions = Some(CheckpointPermissions::read_only(path.parent().unwrap()));
            }
        }
        self.outputs[self.calls.len() - 1].clone()
    }
}

/// Grounds returned-vs-failed and derived presentation in actual provider HTTP
/// requests and disk restore. An untyped tool's error-looking text stays its
/// observation, while the host's bounded large-output view retains its source.
#[tokio::test]
#[serial_test::serial]
async fn all_four_completed_batches_preserve_observed_errors_and_large_sources() {
    let _launch = ConfinedLaunch::new();
    for wire in ["ollama", "openai", "anthropic", "responses"] {
        let server = MockServer::start().await;
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        Mock::given(method("POST"))
            .respond_with(move |_: &Request| {
                let reply = if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                    batch_reply(wire)
                } else {
                    answer_reply(wire)
                };
                ResponseTemplate::new(200).set_body_json(reply)
            })
            .expect(2)
            .mount(&server)
            .await;
        let workspace = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let harness = SmartHarness::new(
            agent_harness::Session::open(directory.path(), Default::default()).unwrap(),
            Arc::new(|_| Box::pin(async { Ok("\"answer\"".into()) })),
            Default::default(),
        )
        .unwrap();
        let large = "retained tool output\n".repeat(1000);
        let spoof = "Error: frame isolation: Error: re_read refused: this is actual tool output";
        let mut mcp = ReturningBatch {
            calls: Vec::new(),
            outputs: vec![large.clone(), spoof.into(), String::new()],
            effect: workspace.path().join("actual-a.txt"),
            fail_checkpoint: None,
            last_checkpoint: None,
            permissions: None,
        };
        let caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
        let allow = vec![TOOL.to_owned()];
        let uri = server.uri();
        let current = msgs();
        let mut context = ctx(&uri, &current, &caveats);
        context.workspace = workspace.path().to_str().unwrap();
        context.smart_harness = Some(&harness);
        context.persona_tools = Some(&allow);
        context.kind = backend(wire);
        let result = if wire == "responses" {
            openai_responses_complete(context, &mut mcp).await
        } else {
            chat_complete(context, &mut mcp).await
        }
        .unwrap();
        assert_eq!(result.0, "Recovery reviewed.");
        assert_eq!(mcp.calls, ["a", "b", "c"]);
        assert_eq!(std::fs::read_to_string(&mcp.effect).unwrap(), large);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(harness.replay_last_request().unwrap(), requests[1].body);
        assert_request_closure(&body_json(&requests[1]), wire);
        assert!(String::from_utf8_lossy(&requests[1].body).contains(spoof));
        let head = harness.head().unwrap();
        drop(harness);
        let mut restored =
            agent_harness::Session::restore(directory.path(), head, "local-session").unwrap();
        let ids = invocation_ids(directory.path(), head);
        let store = agent_harness::store::FrameStore::open(directory.path()).unwrap();
        for (index, id) in ids.into_iter().enumerate() {
            let call = restored.tool_call(id).unwrap();
            assert_eq!(
                call.state,
                agent_harness::ToolCallState::Returned,
                "{wire} slot {index}"
            );
            let raw = agent_harness::forensics::inspect_from_store(
                &store,
                call.returned.unwrap(),
                Default::default(),
            )
            .unwrap()
            .unwrap();
            assert_eq!(raw.record["origin"], "tool");
            let delivery = agent_harness::forensics::inspect_from_store(
                &store,
                call.delivery.unwrap(),
                Default::default(),
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                delivery.record["origin"],
                if index == 0 { "harness" } else { "tool" }
            );
        }
        let source = restored
            .tool_call(invocation_ids(directory.path(), head)[0])
            .unwrap()
            .returned
            .unwrap();
        let slice = restored
            .re_read(&source.to_string(), large.len() - 20, 20)
            .unwrap();
        assert_eq!(slice["text"], &large[large.len() - 20..]);
    }
}

/// Grounds the completion writer's failure path in an actual unpublishable
/// checkpoint: A's file effect and returned error remain visible to the host,
/// B is never polled, and cold recovery records uncertainty without replay.
#[tokio::test]
#[serial_test::serial]
async fn all_four_persistence_failures_preserve_return_and_stop_before_next_tool() {
    let _launch = ConfinedLaunch::new();
    for wire in ["ollama", "openai", "anthropic", "responses"] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(batch_reply(wire)))
            .expect(1)
            .mount(&server)
            .await;
        let workspace = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let session = agent_harness::Session::open(directory.path(), Default::default()).unwrap();
        let checkpoint = session.checkpoint_path().unwrap();
        let harness = SmartHarness::new(
            session,
            Arc::new(|_| panic!("failure must stop before inference")),
            Default::default(),
        )
        .unwrap();
        let mut mcp = ReturningBatch {
            calls: Vec::new(),
            outputs: vec![COMPLETED.into()],
            effect: workspace.path().join("actual-a.txt"),
            fail_checkpoint: Some(checkpoint.clone()),
            last_checkpoint: None,
            permissions: None,
        };
        let caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
        let allow = vec![TOOL.to_owned()];
        let uri = server.uri();
        let current = msgs();
        let mut context = ctx(&uri, &current, &caveats);
        context.workspace = workspace.path().to_str().unwrap();
        context.smart_harness = Some(&harness);
        context.persona_tools = Some(&allow);
        context.kind = backend(wire);
        let error = if wire == "responses" {
            openai_responses_complete(context, &mut mcp).await
        } else {
            chat_complete(context, &mut mcp).await
        }
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains(COMPLETED),
            "host error retains the actual disclosed return: {text}"
        );
        assert!(text.contains("tool completion failed for"), "{text}");
        assert_eq!(mcp.calls, ["a"]);
        assert_eq!(std::fs::read_to_string(&mcp.effect).unwrap(), COMPLETED);
        assert!(harness.head().is_err());
        drop(harness);
        // Restore the exact last published checkpoint after the injected disk
        // obstruction is removed, without accepting any unpublished event.
        drop(mcp.permissions.take());
        let head_text = mcp.last_checkpoint.unwrap();
        assert_eq!(
            std::fs::read_to_string(&checkpoint).unwrap(),
            head_text,
            "failed publication preserves the previous head"
        );
        let head = head_text.trim().parse().unwrap();
        let restored =
            agent_harness::Session::restore(directory.path(), head, "local-session").unwrap();
        let ids = invocation_ids(directory.path(), head);
        assert_eq!(
            restored.tool_call(ids[0]).unwrap().state,
            agent_harness::ToolCallState::Uncertain
        );
        for id in &ids[1..] {
            assert_eq!(
                restored.tool_call(*id).unwrap().state,
                agent_harness::ToolCallState::NotStarted
            );
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

/// Grounds abrupt caller-future abandonment in real storage without relying on
/// the provider's ordinary cancellation return path or replaying old calls.
#[tokio::test]
#[serial_test::serial]
async fn all_four_abrupt_future_drops_preserve_completed_calls() {
    for wire in ["ollama", "openai", "anthropic", "responses"] {
        recovered_after_sibling(wire, true, false).await;
    }
}

/// Grounds the live SpillStore bridge at the same cancellation boundary: a
/// fresh session retrieves the full source after the old spill store is gone.
#[tokio::test]
#[serial_test::serial]
async fn all_four_spill_backed_returns_survive_cancelled_siblings() {
    for wire in ["ollama", "openai", "anthropic", "responses"] {
        recovered_after_sibling(wire, false, true).await;
    }
}

/// Grounds the supported OpenAI proxy dialect that retains flat Anthropic
/// input blocks: admission and cold continuation keep the original envelope.
#[tokio::test]
#[serial_test::serial]
async fn openai_proxy_native_batch_preserves_completed_calls() {
    recovered_after_sibling("openai_native", false, false).await;
}

fn assert_request_closure(body: &serde_json::Value, wire: &str) {
    let ids = ["call_a", "call_b", "call_c"];
    if wire == "responses" {
        let input = body["input"].as_array().unwrap();
        let first = input
            .iter()
            .position(|item| item["type"] == "function_call")
            .unwrap();
        assert!(first > 0);
        assert_eq!(input[first - 1]["type"], "reasoning");
        assert_eq!(input[first - 1]["id"], "reason_batch");
        for (index, id) in ids.into_iter().enumerate() {
            let call = &input[first + index];
            assert_eq!(call["type"], "function_call");
            assert_eq!(call["call_id"], id);
            assert_eq!(call["id"], format!("fc_{}", ["a", "b", "c"][index]));
            let output = &input[first + 3 + index];
            assert_eq!(
                output["type"], "function_call_output",
                "no interleaved operator message before closure"
            );
            assert_eq!(output["call_id"], id);
        }
    } else if wire == "anthropic" {
        let messages = body["messages"].as_array().unwrap();
        let first = messages
            .iter()
            .position(|message| {
                message["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_use"))
            })
            .unwrap();
        assert_eq!(messages[first]["role"], "assistant");
        let calls = messages[first]["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|block| block["type"] == "tool_use")
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 3);
        assert_eq!(messages[first + 1]["role"], "user");
        let outputs = messages[first + 1]["content"].as_array().unwrap();
        assert_eq!(
            outputs
                .iter()
                .filter(|block| block["type"] == "tool_result")
                .count(),
            3,
            "all results form one adjacent native group"
        );
        for (index, id) in ids.into_iter().enumerate() {
            assert_eq!(calls[index]["id"], id);
            assert_eq!(outputs[index]["type"], "tool_result");
            assert_eq!(outputs[index]["tool_use_id"], id);
        }
        assert!(
            outputs[3..].iter().all(|block| block["type"] == "text"),
            "coalesced operator text may follow, never split, the result group"
        );
    } else {
        let messages = body["messages"].as_array().unwrap();
        let first = messages
            .iter()
            .position(|message| {
                message["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty())
            })
            .unwrap();
        assert_eq!(messages[first]["role"], "assistant");
        let calls = messages[first]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 3);
        for (index, id) in ids.into_iter().enumerate() {
            let output = &messages[first + 1 + index];
            assert_eq!(
                output["role"], "tool",
                "no interleaved operator message before closure"
            );
            if wire == "ollama" {
                assert_eq!(
                    calls[index]["function"]["arguments"]["stage"],
                    ["a", "b", "c"][index]
                );
            } else {
                assert_eq!(calls[index]["id"], id);
                assert_eq!(output["tool_call_id"], id);
            }
        }
    }
}

/// Grounds source-retention errors after a real A effect at each provider's
/// return boundary: the raw return survives, B/C never start, and the host gets
/// the actual result plus the reason its claimed spill source was refused.
#[tokio::test]
#[serial_test::serial]
async fn all_four_bad_spill_returns_remain_observed_without_scheduling_siblings() {
    for wire in ["ollama", "openai", "anthropic", "responses"] {
        bad_spill_return(wire, false, false).await;
    }
}

async fn bad_spill_return(wire: &str, malformed: bool, resume: bool) {
    use crate::agentic::content_spill::{SessionSpillStore, SpillProvenance, SpillStore};
    let _launch = ConfinedLaunch::new();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(batch_reply(wire)))
        .expect(1)
        .mount(&server)
        .await;
    let workspace = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let store = SessionSpillStore::new([4; 16]);
    let uncommitted = store
        .stage(
            SpillProvenance::ToolOutput {
                tool_name: Some(TOOL.into()),
            },
            "absent source".into(),
        )
        .unwrap();
    let handle = if malformed {
        "b".repeat(59)
    } else {
        uncommitted.handle()
    };
    let raw = format!(
        "{COMPLETED}\n{}",
        crate::agentic::content_spill::tool_output_retrieval_hint(&handle)
    );
    let harness = SmartHarness::new(
        agent_harness::Session::open(directory.path(), Default::default()).unwrap(),
        Arc::new(|_| panic!("source error must stop")),
        Default::default(),
    )
    .unwrap();
    let mut mcp = ReturningBatch {
        calls: Vec::new(),
        outputs: vec![raw.clone()],
        effect: workspace.path().join("actual-a.txt"),
        fail_checkpoint: None,
        last_checkpoint: None,
        permissions: None,
    };
    let caveats = crate::confined_exec::workspace_confined_caveats(workspace.path());
    let allow = vec![TOOL.to_owned()];
    let uri = server.uri();
    let current = msgs();
    let mut context = ctx(&uri, &current, &caveats);
    context.workspace = workspace.path().to_str().unwrap();
    context.smart_harness = Some(&harness);
    context.persona_tools = Some(&allow);
    context.spill_store = Some(&store);
    context.kind = backend(wire);
    let error = if wire == "responses" {
        openai_responses_complete(context, &mut mcp).await
    } else {
        chat_complete(context, &mut mcp).await
    }
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains(&raw), "{wire}: {error}");
    let cause = if malformed {
        "handle is not a valid content-address"
    } else {
        "retained tool output is absent from its authorized spill store"
    };
    assert!(error.contains(cause), "{wire}: {error}");
    assert_eq!(mcp.calls, ["a"]);
    assert_eq!(std::fs::read_to_string(&mcp.effect).unwrap(), raw);
    let head = harness.head().unwrap();
    drop(harness);
    drop(store);
    let ids = invocation_ids(directory.path(), head);
    let mut restored =
        agent_harness::Session::restore(directory.path(), head, "local-session").unwrap();
    let call = restored.tool_call(ids[0]).unwrap();
    assert_eq!(
        call.state,
        agent_harness::ToolCallState::Returned,
        "{wire}: observed A is not uncertain"
    );
    let returned = call.returned.unwrap();
    for id in &ids[1..] {
        assert_eq!(
            restored.tool_call(*id).unwrap().state,
            agent_harness::ToolCallState::NotStarted
        );
    }
    assert_eq!(
        restored.re_read(&returned.to_string(), 0, 4096).unwrap()["text"],
        raw
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    if resume {
        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer_reply(wire)))
            .expect(1)
            .mount(&server)
            .await;
        let harness = SmartHarness::new(
            restored,
            Arc::new(|_| Box::pin(async { Ok("\"answer\"".into()) })),
            Default::default(),
        )
        .unwrap();
        let mut context = ctx(&uri, &current, &caveats);
        context.workspace = workspace.path().to_str().unwrap();
        context.smart_harness = Some(&harness);
        context.persona_tools = Some(&allow);
        context.kind = backend(wire);
        let resumed = openai_responses_complete(context, &mut mcp)
            .await
            .expect("a source-retention error must allow a legal operator continuation");
        assert_eq!(resumed.0, "Recovery reviewed.");
        assert_eq!(
            mcp.calls,
            ["a"],
            "continuation must not replay old side effects"
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_request_closure(&body_json(&requests[0]), wire);
        assert_eq!(harness.replay_last_request().unwrap(), requests[0].body);
        let dispatched = String::from_utf8_lossy(&requests[0].body);
        assert!(
            dispatched.contains(cause),
            "the recovery notice retains the source failure reason"
        );
        assert!(
            !dispatched.contains(&format!("spill:{handle}")),
            "untrusted raw markers stay in the separately retained observation"
        );
    }
}

/// Grounds missing-source recovery in actual Responses HTTP continuation after
/// cold restore: the failure reason must not copy raw unresolved wire markers.
#[tokio::test]
#[serial_test::serial]
async fn responses_missing_spill_failure_allows_operator_continuation() {
    bad_spill_return("responses", false, true).await;
}

/// Grounds malformed-source recovery in the same actual transport boundary,
/// preserving the original returned bytes without poisoning the next request.
#[tokio::test]
#[serial_test::serial]
async fn responses_malformed_spill_failure_allows_operator_continuation() {
    bad_spill_return("responses", true, true).await;
}
