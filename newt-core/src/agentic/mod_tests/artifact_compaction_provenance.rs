//! Loop-level regressions for prompt-rooted compaction checkpoints.
//!
//! These intentionally exercise the real Ollama loop rather than calling the
//! hook directly: the checkpoint must describe a context transformation which
//! the next model request can actually observe.

use super::*;
use crate::caveats::Caveats;
use crate::{BackendKind, MemMessage, Role};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TASK: &str = "finish the prompt-rooted provenance regression";
const CANNED_SUMMARY: &str = "summary retained for the automatic checkpoint regression";

fn ctx<'a>(server_uri: &'a str, messages: &'a [MemMessage], caveats: &'a Caveats) -> ChatCtx<'a> {
    ChatCtx {
        url: server_uri,
        model: "test-model",
        kind: BackendKind::Ollama,
        api_key: None,
        messages,
        task: TASK,
        workspace: ".",
        color: false,
        markdown: false,
        tool_offload: false,
        spill_store: None,
        compaction_store: None,
        scratchpad: false,
        scratchpad_store: None,
        code_search: None,
        experience_store: None,
        step_ledger: None,
        caveats,
        persona_tools: None,
        max_tool_rounds: 4,
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
    serde_json::from_slice(&req.body).expect("provider request is JSON")
}

fn is_stream(req: &Request) -> bool {
    body_json(req)["stream"].as_bool().unwrap_or(false)
}

fn ndjson(lines: &[serde_json::Value]) -> ResponseTemplate {
    let body = lines
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/x-ndjson")
}

fn request_contains(body: &serde_json::Value, needle: &str) -> bool {
    body["messages"].as_array().is_some_and(|messages| {
        messages.iter().any(|message| {
            message["content"]
                .as_str()
                .is_some_and(|content| content.contains(needle))
        })
    })
}

fn automatic_compaction_messages() -> Vec<MemMessage> {
    let filler = "automatic checkpoint payload that will be summarized. ".repeat(80);
    let mut messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("an earlier operator request"),
    ];
    for _ in 0..12 {
        messages.push(MemMessage::assistant(format!(
            "historical result: {filler}"
        )));
        messages.push(MemMessage::user("continue the historical work"));
    }
    messages.push(MemMessage::user(TASK));
    messages
}

fn overflow_fallback_messages() -> Vec<MemMessage> {
    let mut messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("historical tool work"),
        MemMessage::assistant("the old tool result follows"),
        MemMessage {
            role: Role::Tool,
            content: "aged tool payload which must be structurally pruned\n".repeat(700),
        },
    ];
    // Keep the giant result outside prune's protected ten-message tail while
    // preserving a real current user task at the end of the conversation.
    for _ in 0..9 {
        messages.push(MemMessage::user("historical tail marker"));
    }
    messages.push(MemMessage::user(TASK));
    messages
}

fn assert_one_checkpoint(
    artifacts: &SessionArtifactStore,
    turn: &crate::TurnPromptContext,
    action: &str,
    reason: &str,
) {
    let page = artifacts
        .list_for_root(turn.active_operator_prompt().root_prompt_id(), 0, 10)
        .expect("artifact page");
    let checkpoints: Vec<_> = page
        .records
        .iter()
        .filter(|record| record.kind == "compaction_checkpoint")
        .collect();
    assert_eq!(
        checkpoints.len(),
        1,
        "one transformed working set must yield one checkpoint: {:#?}",
        page.records
    );
    assert_eq!(
        page.total, 1,
        "no unrelated lifecycle artifact belongs here"
    );
    let record = checkpoints[0];
    assert_eq!(record.prompt_id, turn.submitted_prompt().id());
    assert_eq!(
        record.root_prompt_id,
        turn.active_operator_prompt().root_prompt_id()
    );
    assert_eq!(record.metadata["action"], action);
    assert_eq!(record.metadata["reason"], reason);
    let before = record.metadata["tokens_before"]
        .as_u64()
        .expect("checkpoint records numeric tokens_before");
    let after = record.metadata["tokens_after"]
        .as_u64()
        .expect("checkpoint records numeric tokens_after");
    assert!(
        before > after,
        "checkpoint must describe a real reclaim: {}",
        record.metadata
    );
}

fn canned_summarizer(calls: Arc<AtomicUsize>) -> Summarizer {
    Box::new(move |_prompt| {
        let calls = calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(CANNED_SUMMARY.to_string())
        })
    })
}

/// A normal automatic compression must insert its summary before the request
/// is sent, and persist exactly one matching checkpoint for that transformation.
struct AutomaticCheckpointResponder {
    marker_seen: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
}

impl Respond for AutomaticCheckpointResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        self.requests.fetch_add(1, Ordering::SeqCst);
        self.marker_seen.fetch_and(
            request_contains(&body, SUMMARY_PREFIX) && request_contains(&body, CANNED_SUMMARY),
            Ordering::SeqCst,
        );
        if is_stream(req) {
            ndjson(&[serde_json::json!({
                "message": {"content": "automatic checkpoint complete"},
                "done": true,
                "prompt_eval_count": 100,
                "eval_count": 4,
            })])
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "automatic checkpoint complete"},
                "prompt_eval_count": 100,
                "eval_count": 4,
            }))
        }
    }
}

/// A normal final-response mock that records whether the request retained the
/// immutable active prompt or was transformed by the compressor. It lets the
/// regression prove the default policy does not summarize a roomy, known
/// context merely because its message count crossed the legacy threshold.
struct HeadroomRetentionResponder {
    active_prompt_seen: Arc<AtomicBool>,
    compaction_marker_seen: Arc<AtomicBool>,
}

impl Respond for HeadroomRetentionResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        self.active_prompt_seen
            .fetch_or(request_contains(&body, TASK), Ordering::SeqCst);
        self.compaction_marker_seen
            .fetch_or(request_contains(&body, SUMMARY_PREFIX), Ordering::SeqCst);
        if is_stream(req) {
            ndjson(&[serde_json::json!({
                "message": {"content": "headroom retained the active prompt"},
                "done": true,
                "prompt_eval_count": 100,
                "eval_count": 4,
            })])
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "headroom retained the active prompt"},
                "prompt_eval_count": 100,
                "eval_count": 4,
            }))
        }
    }
}

#[tokio::test]
async fn headroom_aware_policy_defers_count_only_compaction_for_known_roomy_window() {
    let server = MockServer::start().await;
    let active_prompt_seen = Arc::new(AtomicBool::new(false));
    let compaction_marker_seen = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(HeadroomRetentionResponder {
            active_prompt_seen: active_prompt_seen.clone(),
            compaction_marker_seen: compaction_marker_seen.clone(),
        })
        .mount(&server)
        .await;

    let messages = automatic_compaction_messages();
    let caveats = Caveats::top();
    let uri = server.uri();
    let turn =
        crate::TurnPromptContext::ephemeral_operator("headroom-aware-count-deferral", TASK, TASK);
    let artifacts = SessionArtifactStore::new("headroom-aware-count-deferral").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let summarizer = canned_summarizer(calls.clone());
    let mut compress_state = CompressState::new();
    let mut c = ctx(&uri, &messages, &caveats);
    // Count exceeds this deliberately low legacy threshold, but the known
    // one-million-token window leaves ample headroom. The default policy must
    // retain the original active prompt and skip artifact creation entirely.
    c.mid_loop_trim_threshold = 20;
    c.safe_context = Some(1_000_000);
    c.summarizer = Some(&*summarizer);
    c.compress_state = Some(&mut compress_state);

    let (reply, _, _, _) = chat_complete_with_prompt_and_artifacts(
        c,
        Some(&turn),
        None,
        Some(&artifacts),
        Some(&artifacts),
        &mut NoMcp,
    )
    .await
    .expect("roomy count-only loop succeeds");

    assert_eq!(reply, "headroom retained the active prompt");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no summarizer invocation");
    assert!(active_prompt_seen.load(Ordering::SeqCst));
    assert!(
        !compaction_marker_seen.load(Ordering::SeqCst),
        "a count-only deferral must not install a compaction summary"
    );
    assert_eq!(
        artifacts
            .list_for_root(turn.active_operator_prompt().root_prompt_id(), 0, 10)
            .unwrap()
            .total,
        0,
        "a deferred decision is not an artifact-worthy transformation"
    );
}

#[tokio::test]
async fn legacy_message_count_policy_still_compacts_under_the_same_roomy_window() {
    let server = MockServer::start().await;
    let marker_seen = Arc::new(AtomicBool::new(true));
    let requests = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(AutomaticCheckpointResponder {
            marker_seen: marker_seen.clone(),
            requests: requests.clone(),
        })
        .mount(&server)
        .await;

    let messages = automatic_compaction_messages();
    let caveats = Caveats::top();
    let uri = server.uri();
    let turn =
        crate::TurnPromptContext::ephemeral_operator("legacy-message-count-policy", TASK, TASK);
    let artifacts = SessionArtifactStore::new("legacy-message-count-policy").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let summarizer = canned_summarizer(calls.clone());
    let mut compress_state = CompressState::new();
    let mut c = ctx(&uri, &messages, &caveats);
    c.mid_loop_trim_threshold = 20;
    c.compaction_trigger_policy = crate::CompactionTriggerPolicy::MessageCount;
    c.safe_context = Some(1_000_000);
    c.summarizer = Some(&*summarizer);
    c.compress_state = Some(&mut compress_state);

    let (reply, _, _, _) = chat_complete_with_prompt_and_artifacts(
        c,
        Some(&turn),
        None,
        Some(&artifacts),
        Some(&artifacts),
        &mut NoMcp,
    )
    .await
    .expect("legacy count-only loop succeeds");

    assert_eq!(reply, "automatic checkpoint complete");
    assert!(calls.load(Ordering::SeqCst) >= 1);
    assert!(marker_seen.load(Ordering::SeqCst));
    assert!(requests.load(Ordering::SeqCst) >= 1);
    assert_one_checkpoint(&artifacts, &turn, "summarized", "automatic_message_count");
    let page = artifacts
        .list_for_root(turn.active_operator_prompt().root_prompt_id(), 0, 10)
        .unwrap();
    let checkpoint = page
        .records
        .iter()
        .find(|record| record.kind == "compaction_checkpoint")
        .unwrap();
    assert_eq!(checkpoint.metadata["trigger"]["policy"], "message_count");
    assert_eq!(
        checkpoint.metadata["trigger"]["primary_cause"],
        "message_count"
    );
    assert_eq!(
        checkpoint.metadata["trigger"]["send_budget_authoritative"],
        true
    );
}

#[tokio::test]
async fn automatic_compaction_records_one_checkpoint_after_installing_summary() {
    let server = MockServer::start().await;
    let marker_seen = Arc::new(AtomicBool::new(true));
    let requests = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(AutomaticCheckpointResponder {
            marker_seen: marker_seen.clone(),
            requests: requests.clone(),
        })
        .mount(&server)
        .await;

    let messages = automatic_compaction_messages();
    let caveats = Caveats::top();
    let uri = server.uri();
    let turn =
        crate::TurnPromptContext::ephemeral_operator("automatic-compaction-artifacts", TASK, TASK);
    let artifacts = SessionArtifactStore::new("automatic-compaction-artifacts").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let summarizer = canned_summarizer(calls.clone());
    let mut compress_state = CompressState::new();
    let mut c = ctx(&uri, &messages, &caveats);
    // This is a normal automatic hard-token trigger, not a manual compaction.
    c.mid_loop_trim_tokens = Some(9_400);
    c.summarizer = Some(&*summarizer);
    c.compress_state = Some(&mut compress_state);

    let (reply, _, _, _) = chat_complete_with_prompt_and_artifacts(
        c,
        Some(&turn),
        None,
        Some(&artifacts),
        Some(&artifacts),
        &mut NoMcp,
    )
    .await
    .expect("automatic compaction loop succeeds");

    assert_eq!(reply, "automatic checkpoint complete");
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "the automatic transformation must invoke its configured summarizer"
    );
    assert!(
        marker_seen.load(Ordering::SeqCst),
        "the provider must observe the installed summary marker on the transformed request"
    );
    assert!(requests.load(Ordering::SeqCst) >= 1);
    assert_one_checkpoint(&artifacts, &turn, "summarized", "automatic_token_threshold");
    let page = artifacts
        .list_for_root(turn.active_operator_prompt().root_prompt_id(), 0, 10)
        .unwrap();
    let checkpoint = page
        .records
        .iter()
        .find(|record| record.kind == "compaction_checkpoint")
        .unwrap();
    assert!(checkpoint.body.as_deref().unwrap().contains(&format!(
        "root:{}",
        turn.active_operator_prompt().root_prompt_id()
    )));
    assert_eq!(checkpoint.metadata["trigger"]["policy"], "headroom_aware");
    assert_eq!(
        checkpoint.metadata["trigger"]["primary_cause"],
        "token_threshold"
    );
    assert_eq!(
        checkpoint.metadata["trigger"]["causes"]["token_threshold"],
        true
    );
}

/// First probe + stream are both empty at the synthetic 85%-of-window mark.
/// The working set itself fits the retry compressor's target, so the loop must
/// take its one structural fallback and the next provider request must see the
/// pruned tool result.
struct SilentOverflowFallbackResponder {
    probes: Arc<AtomicUsize>,
    pruned_request_seen: Arc<AtomicBool>,
    request_log: Arc<Mutex<Vec<bool>>>,
}

impl Respond for SilentOverflowFallbackResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let is_pruned = request_contains(&body, "[tool] result elided -> ok");
        self.pruned_request_seen
            .fetch_or(is_pruned, Ordering::SeqCst);
        self.request_log.lock().unwrap().push(is_pruned);

        if is_stream(req) {
            if self.probes.load(Ordering::SeqCst) <= 1 {
                return ndjson(&[serde_json::json!({
                    "message": {"content": ""},
                    "done": true,
                    "prompt_eval_count": 85_000,
                    "eval_count": 0,
                })]);
            }
            return ndjson(&[serde_json::json!({
                "message": {"content": "fallback checkpoint complete"},
                "done": true,
                "prompt_eval_count": 100,
                "eval_count": 3,
            })]);
        }

        let probe = self.probes.fetch_add(1, Ordering::SeqCst);
        if probe == 0 {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": ""},
                "prompt_eval_count": 85_000,
                "eval_count": 0,
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "fallback checkpoint complete"},
                "prompt_eval_count": 100,
                "eval_count": 3,
            }))
        }
    }
}

#[tokio::test]
async fn ollama_silent_overflow_structural_fallback_records_one_checkpoint() {
    let server = MockServer::start().await;
    let probes = Arc::new(AtomicUsize::new(0));
    let pruned_request_seen = Arc::new(AtomicBool::new(false));
    let request_log = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(SilentOverflowFallbackResponder {
            probes: probes.clone(),
            pruned_request_seen: pruned_request_seen.clone(),
            request_log: request_log.clone(),
        })
        .mount(&server)
        .await;

    let messages = overflow_fallback_messages();
    let caveats = Caveats::top();
    let uri = server.uri();
    let turn =
        crate::TurnPromptContext::ephemeral_operator("silent-overflow-artifacts", TASK, TASK);
    let artifacts = SessionArtifactStore::new("silent-overflow-artifacts").unwrap();
    let mut c = ctx(&uri, &messages, &caveats);
    // A high window leaves the retry compressor's target much larger than the
    // local message estimate. The fake 85k provider report is therefore what
    // drives the silent-overflow path, while `compress` itself returns Fit.
    c.safe_context = Some(100_000);

    let (reply, _, _, _) = chat_complete_with_prompt_and_artifacts(
        c,
        Some(&turn),
        None,
        Some(&artifacts),
        Some(&artifacts),
        &mut NoMcp,
    )
    .await
    .expect("silent-overflow fallback loop succeeds");

    assert_eq!(reply, "fallback checkpoint complete");
    assert_eq!(
        probes.load(Ordering::SeqCst),
        2,
        "one empty retry then recovery"
    );
    assert!(
        pruned_request_seen.load(Ordering::SeqCst),
        "the request after fallback must contain the structurally pruned tool result; log={:?}",
        request_log.lock().unwrap()
    );
    assert_one_checkpoint(
        &artifacts,
        &turn,
        "pruned",
        "silent_overflow_structural_fallback",
    );
}
