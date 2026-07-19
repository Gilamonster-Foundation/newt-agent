use super::*;
use crate::caveats::{Caveats, CountBound, Scope};
use crate::{BackendKind, MemMessage};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TASK: &str = "record the completed plan against this exact prompt";
const PLAN_ARGS: &str =
    r#"{"plan":[{"step":"capture prompt-rooted provenance","status":"completed"}]}"#;

fn messages(task: &str) -> Vec<MemMessage> {
    vec![MemMessage::system("you are a test"), MemMessage::user(task)]
}

fn ctx<'a>(
    server_uri: &'a str,
    messages: &'a [MemMessage],
    task: &'a str,
    workspace: &'a str,
    caveats: &'a Caveats,
) -> ChatCtx<'a> {
    ChatCtx {
        url: server_uri,
        model: "test-model",
        kind: BackendKind::Ollama,
        api_key: None,
        messages,
        task,
        workspace,
        color: false,
        markdown: false,
        tool_offload: false,
        spill_store: None,
        compaction_store: None,
        scratchpad: false,
        scratchpad_store: None,
        code_search: None,
        where_is: None,
        experience_store: None,
        step_ledger: None,
        caveats,
        persona_tools: None,
        max_tool_rounds: 5,
        narration_nudge_cap: 0,
        action_nudges: false,
        prompt_disposition: PromptDisposition::Act,
        prompt_intake: None,
        workflow_grace_rounds: 0,
        tool_output_lines: 20,
        debug: false,
        trace: false,
        num_ctx: None,
        input_ceiling_pct: 80,
        low_budget_pct: 15,
        connect_timeout_secs: 5,
        inference_timeout_secs: 30,
        mid_loop_trim_threshold: 40,
        compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
        mid_loop_trim_tokens: None,
        max_ok_input: None,
        build_check_cmd: None,
        safe_context: None,
        recover_cw_400: None,
        note_sink: None,
        note_nudge: None,
        recall_source: None,
        memory_source: None,
        summarizer: None,
        compress_state: None,
        tool_events: None,
        phantom_reaches: None,
        end_reason: None,
        permission_gate: None,
        on_round_usage: None,
        estimate_ratio: None,
        estimation: crate::tokens::TokenEstimation::default(),
        summary_input_cap_floor_chars: 8_192,
        exec_floor: None,
        write_ledger: None,
        cancel: None,
        live_tool_output: None,
        git_tool: None,
        crew_runner: None,
    }
}

fn body_json(req: &Request) -> serde_json::Value {
    serde_json::from_slice(&req.body).expect("provider request body is JSON")
}

fn chat_tools_include(body: &serde_json::Value, name: &str) -> bool {
    body["tools"].as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool["function"]["name"].as_str() == Some(name))
    })
}

fn responses_tools_include(body: &serde_json::Value, name: &str) -> bool {
    body["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["name"].as_str() == Some(name)))
}

fn ndjson(lines: &[serde_json::Value]) -> ResponseTemplate {
    let body = lines
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/x-ndjson")
}

fn assert_one_plan_artifact(artifacts: &SessionArtifactStore, turn: &crate::TurnPromptContext) {
    let page = artifacts
        .list_for_root(turn.active_operator_prompt().root_prompt_id(), 0, 10)
        .expect("artifact root page");
    assert_eq!(page.total, 1, "one successful plan update, one artifact");
    let record = &page.records[0];
    assert_eq!(record.kind, "plan_revision");
    assert_eq!(record.prompt_id, turn.submitted_prompt().id());
    assert_eq!(
        record.root_prompt_id,
        turn.active_operator_prompt().root_prompt_id()
    );
    assert_eq!(record.locator.as_deref(), Some("plan"));
    assert!(
        record
            .body
            .as_deref()
            .is_some_and(|body| body.contains("capture prompt-rooted provenance")),
        "bounded normalized plan body is retained"
    );
}

struct OllamaPlanResponder {
    requests: Arc<AtomicUsize>,
    artifact_read_seen: Arc<AtomicBool>,
}

impl Respond for OllamaPlanResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        self.artifact_read_seen
            .fetch_and(chat_tools_include(&body, "artifact_read"), Ordering::SeqCst);
        let round = self.requests.fetch_add(1, Ordering::SeqCst);
        match round {
            0 => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "function": {
                            "name": "update_plan",
                            "arguments": serde_json::from_str::<serde_json::Value>(PLAN_ARGS).unwrap()
                        }
                    }]
                },
                "prompt_eval_count": 20,
                "eval_count": 3
            })),
            1 => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"role": "assistant", "content": "plan provenance captured"},
                "prompt_eval_count": 24,
                "eval_count": 4
            })),
            _ => ndjson(&[serde_json::json!({
                "message": {"role": "assistant", "content": "plan provenance captured"},
                "done": true,
                "prompt_eval_count": 24,
                "eval_count": 4
            })]),
        }
    }
}

#[tokio::test]
async fn ollama_advertises_artifact_read_and_records_plan_provenance() {
    let server = MockServer::start().await;
    let requests = Arc::new(AtomicUsize::new(0));
    let artifact_read_seen = Arc::new(AtomicBool::new(true));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OllamaPlanResponder {
            requests: requests.clone(),
            artifact_read_seen: artifact_read_seen.clone(),
        })
        .mount(&server)
        .await;

    let turn = crate::TurnPromptContext::ephemeral_operator("ollama-artifacts", TASK, TASK);
    let artifacts = SessionArtifactStore::new("ollama-artifacts").unwrap();
    let ledger = SessionStepLedger::default();
    let messages = messages(TASK);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, TASK, ".", &caveats);
    c.step_ledger = Some(&ledger);

    let (reply, _, _, _) = chat_complete_with_prompt_and_artifacts(
        c,
        Some(&turn),
        None,
        Some(&artifacts),
        Some(&artifacts),
        &mut NoMcp,
    )
    .await
    .expect("Ollama plan loop succeeds");

    assert_eq!(reply, "plan provenance captured");
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    assert!(
        artifact_read_seen.load(Ordering::SeqCst),
        "artifact_read must ride every Ollama inference request"
    );
    assert_one_plan_artifact(&artifacts, &turn);
}

struct OpenAiPlanResponder {
    requests: Arc<AtomicUsize>,
    artifact_read_seen: Arc<AtomicBool>,
}

impl Respond for OpenAiPlanResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        self.artifact_read_seen
            .fetch_and(chat_tools_include(&body, "artifact_read"), Ordering::SeqCst);
        let round = self.requests.fetch_add(1, Ordering::SeqCst);
        if round == 0 {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "plan-call",
                        "type": "function",
                        "function": {"name": "update_plan", "arguments": PLAN_ARGS}
                    }]
                }}],
                "usage": {"prompt_tokens": 20, "completion_tokens": 3}
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "role": "assistant",
                    "content": "retry plan provenance captured"
                }}],
                "usage": {"prompt_tokens": 24, "completion_tokens": 4}
            }))
        }
    }
}

#[tokio::test]
async fn openai_chat_records_harness_retry_against_submitted_not_active_prompt() {
    let server = MockServer::start().await;
    let requests = Arc::new(AtomicUsize::new(0));
    let artifact_read_seen = Arc::new(AtomicBool::new(true));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiPlanResponder {
            requests: requests.clone(),
            artifact_read_seen: artifact_read_seen.clone(),
        })
        .mount(&server)
        .await;

    let operator = crate::TurnPromptContext::ephemeral_operator("chat-artifacts", TASK, TASK);
    let retry = crate::TurnPromptContext::ephemeral_harness_retry(
        "chat-artifacts",
        "retry after provider timeout",
        "retry after provider timeout",
        &operator,
    )
    .unwrap();
    assert_ne!(
        retry.submitted_prompt().id(),
        retry.active_operator_prompt().id()
    );
    let artifacts = SessionArtifactStore::new("chat-artifacts").unwrap();
    let ledger = SessionStepLedger::default();
    let messages = messages(TASK);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, TASK, ".", &caveats);
    c.kind = BackendKind::Openai;
    c.step_ledger = Some(&ledger);

    let (reply, _, _, _) = chat_complete_with_prompt_and_artifacts(
        c,
        Some(&retry),
        None,
        Some(&artifacts),
        Some(&artifacts),
        &mut NoMcp,
    )
    .await
    .expect("OpenAI Chat plan loop succeeds");

    assert_eq!(reply, "retry plan provenance captured");
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert!(
        artifact_read_seen.load(Ordering::SeqCst),
        "artifact_read must ride every Chat Completions request"
    );
    assert_one_plan_artifact(&artifacts, &retry);
    let page = artifacts
        .list_for_root(retry.active_operator_prompt().root_prompt_id(), 0, 10)
        .unwrap();
    assert_ne!(
        page.records[0].prompt_id,
        retry.active_operator_prompt().id(),
        "retry work must not be silently reparented to operator authority"
    );
}

struct ResponsesPlanResponder {
    requests: Arc<AtomicUsize>,
    artifact_read_seen: Arc<AtomicBool>,
}

impl Respond for ResponsesPlanResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        self.artifact_read_seen.fetch_and(
            responses_tools_include(&body, "artifact_read"),
            Ordering::SeqCst,
        );
        let round = self.requests.fetch_add(1, Ordering::SeqCst);
        if round == 0 {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "output": [{
                    "type": "function_call",
                    "call_id": "plan-call",
                    "name": "update_plan",
                    "arguments": PLAN_ARGS
                }],
                "usage": {"input_tokens": 20, "output_tokens": 3}
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Responses provenance captured"}]
                }],
                "usage": {"input_tokens": 24, "output_tokens": 4}
            }))
        }
    }
}

#[tokio::test]
async fn responses_advertises_artifact_read_and_records_plan_provenance() {
    let server = MockServer::start().await;
    let requests = Arc::new(AtomicUsize::new(0));
    let artifact_read_seen = Arc::new(AtomicBool::new(true));
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponsesPlanResponder {
            requests: requests.clone(),
            artifact_read_seen: artifact_read_seen.clone(),
        })
        .mount(&server)
        .await;

    let turn = crate::TurnPromptContext::ephemeral_operator("responses-artifacts", TASK, TASK);
    let artifacts = SessionArtifactStore::new("responses-artifacts").unwrap();
    let ledger = SessionStepLedger::default();
    let messages = messages(TASK);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, TASK, ".", &caveats);
    c.kind = BackendKind::Openai;
    c.step_ledger = Some(&ledger);

    // Call the explicit sibling loop instead of mutating NEWT_OPENAI_API: a
    // process-global env flip would race unrelated OpenAI tests in this crate.
    let (reply, _, _, _) = openai_responses_complete_with_prompt_and_artifacts(
        c,
        Some(&turn),
        None,
        Some(&artifacts),
        Some(&artifacts),
        &mut NoMcp,
    )
    .await
    .expect("Responses plan loop succeeds");

    assert_eq!(reply, "Responses provenance captured");
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert!(
        artifact_read_seen.load(Ordering::SeqCst),
        "artifact_read must ride every Responses request"
    );
    assert_one_plan_artifact(&artifacts, &turn);
}

struct OpenAiScriptResponder {
    requests: Arc<AtomicUsize>,
    first_message: serde_json::Value,
    final_text: &'static str,
}

impl Respond for OpenAiScriptResponder {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let round = self.requests.fetch_add(1, Ordering::SeqCst);
        let message = if round == 0 {
            self.first_message.clone()
        } else {
            serde_json::json!({"role": "assistant", "content": self.final_text})
        };
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": message}],
            "usage": {"prompt_tokens": 20, "completion_tokens": 3}
        }))
    }
}

#[tokio::test]
async fn successful_builtin_write_records_digest_only_file_change() {
    let server = MockServer::start().await;
    let requests = Arc::new(AtomicUsize::new(0));
    const CONTENT: &str = "sensitive fixture bytes that must not enter the artifact";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiScriptResponder {
            requests: requests.clone(),
            first_message: serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "write-call",
                    "type": "function",
                    "function": {
                        "name": "write_file",
                        "arguments": serde_json::json!({
                            "path": "derived.txt",
                            "content": CONTENT
                        }).to_string()
                    }
                }]
            }),
            final_text: "file provenance captured",
        })
        .mount(&server)
        .await;

    let workspace = tempfile::TempDir::new().unwrap();
    let workspace_str = workspace.path().to_str().unwrap();
    let caveats = Caveats {
        fs_read: Scope::All,
        fs_write: Scope::only([workspace_str.to_string()]),
        exec: Scope::none(),
        net: Scope::none(),
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    };
    let turn = crate::TurnPromptContext::ephemeral_operator(
        "write-artifacts",
        "write the fixture",
        "write the fixture",
    );
    let artifacts = SessionArtifactStore::new("write-artifacts").unwrap();
    let messages = messages("write the fixture");
    let uri = server.uri();
    let mut c = ctx(
        &uri,
        &messages,
        "write the fixture",
        workspace_str,
        &caveats,
    );
    c.kind = BackendKind::Openai;

    chat_complete_with_prompt_and_artifacts(
        c,
        Some(&turn),
        None,
        Some(&artifacts),
        Some(&artifacts),
        &mut NoMcp,
    )
    .await
    .expect("write loop succeeds");

    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("derived.txt")).unwrap(),
        CONTENT
    );
    let page = artifacts
        .list_for_root(turn.active_operator_prompt().root_prompt_id(), 0, 10)
        .unwrap();
    assert_eq!(page.total, 1);
    let record = &page.records[0];
    assert_eq!(record.kind, "file_change");
    assert_eq!(record.prompt_id, turn.submitted_prompt().id());
    assert_eq!(record.locator.as_deref(), Some("derived.txt"));
    assert_eq!(
        record.body, None,
        "file bytes never become an artifact body"
    );
    assert_eq!(record.metadata["digest_algorithm"], "blake3");
    assert_eq!(record.metadata["before"]["exists"], false);
    assert_eq!(record.metadata["after"]["exists"], true);
    assert_eq!(
        record.metadata["after"]["digest"],
        blake3::hash(CONTENT.as_bytes()).to_hex().to_string()
    );
    assert_eq!(record.metadata["after"]["bytes"], CONTENT.len() as u64);
    assert!(
        !record.metadata.to_string().contains(CONTENT),
        "metadata contains locators/digests, never file contents"
    );
}

#[tokio::test]
async fn invalid_plan_and_declined_write_record_no_artifacts() {
    let server = MockServer::start().await;
    let requests = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiScriptResponder {
            requests: requests.clone(),
            first_message: serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {
                        "id": "bad-plan",
                        "type": "function",
                        "function": {"name": "update_plan", "arguments": "{}"}
                    },
                    {
                        "id": "declined-write",
                        "type": "function",
                        "function": {
                            "name": "write_file",
                            "arguments": serde_json::json!({
                                "path": "declined.txt",
                                "content": "must not be written"
                            }).to_string()
                        }
                    }
                ]
            }),
            final_text: "nothing was recorded",
        })
        .mount(&server)
        .await;

    let workspace = tempfile::TempDir::new().unwrap();
    let workspace_str = workspace.path().to_str().unwrap();
    // Unrestricted writes require an explicit gate answer. With no gate in
    // the headless context this call is deterministically declined.
    let caveats = Caveats::top();
    let turn = crate::TurnPromptContext::ephemeral_operator(
        "failed-artifacts",
        "try invalid work",
        "try invalid work",
    );
    let artifacts = SessionArtifactStore::new("failed-artifacts").unwrap();
    let ledger = SessionStepLedger::default();
    let messages = messages("try invalid work");
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, "try invalid work", workspace_str, &caveats);
    c.kind = BackendKind::Openai;
    c.step_ledger = Some(&ledger);

    chat_complete_with_prompt_and_artifacts(
        c,
        Some(&turn),
        None,
        Some(&artifacts),
        Some(&artifacts),
        &mut NoMcp,
    )
    .await
    .expect("failed-tool loop still completes");

    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert!(!workspace.path().join("declined.txt").exists());
    assert!(
        ledger.snapshot().is_empty(),
        "invalid plan changed no state"
    );
    let page = artifacts
        .list_for_root(turn.active_operator_prompt().root_prompt_id(), 0, 10)
        .unwrap();
    assert_eq!(page.total, 0, "failed/declined work emits no provenance");
    assert!(page.records.is_empty());
}
