use super::*;
use crate::caveats::Caveats;
use crate::{BackendKind, MemMessage};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TASK: &str =
    "ACTIVE TASK GAUNTLET-7f3d9c: read big.txt until told to stop, then restate this marker";
const CANNED_SUMMARY: &str =
        "## Active Task\nACTIVE TASK GAUNTLET-7f3d9c (canned summary)\n## Completed Actions\n1. read big.txt";

fn msgs() -> Vec<MemMessage> {
    vec![MemMessage::system("you are a test"), MemMessage::user(TASK)]
}

fn ctx<'a>(
    server_uri: &'a str,
    messages: &'a [MemMessage],
    caveats: &'a Caveats,
    workspace: &'a str,
) -> ChatCtx<'a> {
    ChatCtx {
        url: server_uri,
        model: "test-model",
        kind: BackendKind::Ollama,
        api_key: None,
        messages,
        task: TASK,
        workspace,
        color: false,
        markdown: false,
        tool_offload: false,
        spill_store: None,
        disclosure: None,
        compaction_store: None,
        scratchpad: false,
        scratchpad_store: None,
        code_search: None,
        where_is: None,
        nav: None,
        exposure: Default::default(),
        experience_store: None,
        step_ledger: None,
        caveats,
        persona_tools: None,
        cognition: None,
        chat_completions_capability: Default::default(),
        reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
        emits_leading_reasoning: false,
        max_tool_rounds: 12,
        narration_nudge_cap: 1,
        action_nudges: true,
        prompt_disposition: PromptDisposition::Act,
        prompt_intake: None,
        workflow_grace_rounds: 0,
        tool_output_lines: 2,
        debug: false,
        trace: false,
        num_ctx: None,
        input_ceiling_pct: 80,
        low_budget_pct: 15,
        connect_timeout_secs: 5,
        inference_timeout_secs: 30,
        mid_loop_trim_threshold: 40,
        compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
        // The token trigger under test: well below what a few 4 KB
        // tool results accumulate to.
        mid_loop_trim_tokens: Some(5_000),
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
        solve_obs: None,
        permission_gate: None,
        on_round_usage: None,
        estimate_ratio: None,
        estimation: crate::tokens::TokenEstimation::default(),
        summary_input_cap_floor_chars: 8_192,
        rewrites_history: true,
        exec_floor: None,
        write_ledger: None,
        attribution: None,
        cancel: None,
        live_tool_output: None,
        git_tool: None,
        crew_runner: None,
        operating_mode_control: None,
        plan_mode_control: None,
        steering: None,
        completed_spill_renderer: None,
    }
}

/// Workspace with one ~4 KB file the mock model reads over and over.
fn gauntlet_workspace() -> tempfile::TempDir {
    let ws = tempfile::TempDir::new().unwrap();
    let line = "the quick brown newt compresses context without discarding it\n";
    std::fs::write(ws.path().join("big.txt"), line.repeat(64)).unwrap();
    ws
}

fn body_json(req: &Request) -> serde_json::Value {
    serde_json::from_slice(&req.body).unwrap_or_default()
}

fn messages_contain(body: &serde_json::Value, needle: &str) -> bool {
    body["messages"]
        .as_array()
        .map(|msgs| {
            msgs.iter().any(|m| {
                m["content"]
                    .as_str()
                    .map(|c| c.contains(needle))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// chars/4 estimate of a request's message list (mirrors the loop's
/// fallback estimator) — used to measure the reclaim across requests.
fn body_message_tokens(body: &serde_json::Value) -> usize {
    body["messages"]
        .as_array()
        .map(|msgs| {
            msgs.iter()
                .map(|m| {
                    crate::tokens::TokenEstimation::default()
                        .tokens_for_chars(m.to_string().chars().count())
                })
                .sum()
        })
        .unwrap_or(0)
}

/// chars/4 estimate of the complete model input: messages plus the tool
/// schemas that ride on every tools-enabled request.
fn body_request_tokens(body: &serde_json::Value) -> usize {
    let estimation = crate::tokens::TokenEstimation::default();
    body_message_tokens(body)
        + body
            .get("tools")
            .map(|tools| estimate_value_tokens(tools, estimation))
            .unwrap_or(0)
}

/// A summarizer that records every request it receives and returns the
/// canned summary.
fn canned_summarizer(prompts: Arc<Mutex<Vec<String>>>) -> Summarizer {
    Box::new(move |prompt: String| {
        let prompts = prompts.clone();
        Box::pin(async move {
            prompts.lock().unwrap().push(prompt);
            Ok(CANNED_SUMMARY.to_string())
        })
    })
}

/// Ollama-shaped gauntlet responder: keeps demanding `read_file` of the
/// big fixture until the compaction marker shows up in the request, then
/// answers. Records per-request observations the assertions need.
struct GauntletResponder {
    final_answer: String,
    /// `(had_marker, est_message_tokens, est_full_request_tokens)` per
    /// non-streaming request.
    log: Arc<Mutex<Vec<(bool, usize, usize)>>>,
    task_in_marker_request: Arc<AtomicBool>,
    summary_in_marker_request: Arc<AtomicBool>,
    old_placeholder_seen: Arc<AtomicBool>,
    static_marker_instead: bool,
}

impl Respond for GauntletResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let has_marker = messages_contain(&body, SUMMARY_PREFIX);
        if !body["stream"].as_bool().unwrap_or(false) {
            self.log.lock().unwrap().push((
                has_marker,
                body_message_tokens(&body),
                body_request_tokens(&body),
            ));
        }
        if messages_contain(&body, "earlier tool-call messages omitted") {
            self.old_placeholder_seen.store(true, Ordering::SeqCst);
        }
        if has_marker {
            if messages_contain(&body, TASK) {
                self.task_in_marker_request.store(true, Ordering::SeqCst);
            }
            let summary_needle = if self.static_marker_instead {
                "Summary generation was unavailable."
            } else {
                CANNED_SUMMARY
            };
            if messages_contain(&body, summary_needle) {
                self.summary_in_marker_request.store(true, Ordering::SeqCst);
            }
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": self.final_answer }
            }));
        }
        if body.get("tools").is_some() {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": "", "tool_calls": [{
                    "function": { "name": "read_file", "arguments": { "path": "big.txt" } }
                }]}
            }))
        } else {
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "message": { "content": "cap exit" } }))
        }
    }
}

/// THE B5 acceptance property: compression fires on a long tool-heavy
/// conversation and the original task text still reaches the next
/// request — summarized, not discarded.
#[tokio::test]
async fn active_task_survives_compression() {
    let server = MockServer::start().await;
    let log = Arc::new(Mutex::new(Vec::new()));
    let task_in_marker = Arc::new(AtomicBool::new(false));
    let summary_in_marker = Arc::new(AtomicBool::new(false));
    let old_placeholder = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(GauntletResponder {
            final_answer: "the marker is GAUNTLET-7f3d9c".into(),
            log: log.clone(),
            task_in_marker_request: task_in_marker.clone(),
            summary_in_marker_request: summary_in_marker.clone(),
            old_placeholder_seen: old_placeholder.clone(),
            static_marker_instead: false,
        })
        .mount(&server)
        .await;

    let ws = gauntlet_workspace();
    let workspace = ws.path().to_string_lossy().to_string();
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let summarizer = canned_summarizer(prompts.clone());
    let mut compress_state = CompressState::new();
    let mut c = ctx(&uri, &messages, &caveats, &workspace);
    // The trigger is a complete-request ceiling, including the expanded
    // builtin tool catalog. Derive it as the live catalog weight plus ~5k
    // message tokens of headroom (a catalog-INDEPENDENT offset) so the
    // first zero-cost prune remains observable on wire before the
    // summarizing pass; catalog growth shifts the threshold with it. The
    // >40% reclaim assertion below still measures actual dispatched
    // requests.
    c.mid_loop_trim_tokens = Some(
        builtin_catalog_tokens(PromptDisposition::Act)
            + prompt_read::response_repository_policy_tokens()
            + 5_600,
    );
    c.summarizer = Some(&*summarizer);
    c.compress_state = Some(&mut compress_state);
    let (reply, _streamed, _usage, hallu) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");

    // The turn completed with a real answer, not a cap/diagnostic exit.
    assert_eq!(reply, "the marker is GAUNTLET-7f3d9c");
    assert_eq!(hallu, 0);

    // The summarizer ran exactly once and its request carried the
    // original task verbatim (the verbatim-Active-Task anchor).
    let prompts = prompts.lock().unwrap();
    assert_eq!(prompts.len(), 1, "one compression, one summary request");
    assert!(
        prompts[0].contains(TASK),
        "summary request must quote the task verbatim"
    );

    // The post-compression request still carried the task AND the
    // summary, wrapped in the marker — summarize, don't discard.
    assert!(task_in_marker.load(Ordering::SeqCst), "B5 property");
    assert!(summary_in_marker.load(Ordering::SeqCst));
    assert!(
        !old_placeholder.load(Ordering::SeqCst),
        "the old amputation placeholder must never be dispatched"
    );

    // Reclaim numbers: the compressed request must be materially smaller
    // than the largest pre-compression request.
    let log = log.lock().unwrap();
    let before = log
        .iter()
        .filter(|(m, ..)| !m)
        .map(|&(_, t, _)| t)
        .max()
        .expect("pre-compression requests were dispatched");
    let after = log
        .iter()
        .find(|(m, ..)| *m)
        .map(|&(_, t, _)| t)
        .expect("a compressed request was dispatched");
    println!("e2e reclaim: ~{before} -> ~{after} est. message tokens");
    let fixed = prompt_read::response_repository_policy_tokens();
    let reclaimable_before = before.saturating_sub(fixed);
    let reclaimable_after = after.saturating_sub(fixed);
    assert!(
        reclaimable_after < reclaimable_before * 6 / 10,
        "compression must reclaim >40% of the non-policy messages here \
             (got {before} -> {after}, fixed policy ~{fixed})"
    );
}

/// THE B6 regression (#282): a FIRST-turn request on a fresh capability
/// cache (no `max_ok_input`, no `safe_context`) whose history exceeds the
/// `num_ctx` ceiling must compress BEFORE dispatch and dispatch under the
/// ceiling. Pre-fix, `send_budget` was `None` here — the after-benchmark
/// measured all 10 B6 runs shipping ~41k-token requests into a forced
/// 6,144 window with zero compression events (8/10 silently wrong),
/// because the `num_ctx` newt itself sent fed into nothing.
#[tokio::test]
async fn first_turn_over_num_ctx_ceiling_compresses_before_dispatch() {
    let server = MockServer::start().await;
    let log = Arc::new(Mutex::new(Vec::new()));
    let task_in_marker = Arc::new(AtomicBool::new(false));
    let summary_in_marker = Arc::new(AtomicBool::new(false));
    let old_placeholder = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(GauntletResponder {
            final_answer: "the marker is GAUNTLET-7f3d9c".into(),
            log: log.clone(),
            task_in_marker_request: task_in_marker.clone(),
            summary_in_marker_request: summary_in_marker.clone(),
            old_placeholder_seen: old_placeholder.clone(),
            static_marker_instead: false,
        })
        .mount(&server)
        .await;

    // The B6 shape, condensed: turn 1 of a fresh process already carries
    // a history far over the forced window (a restored conversation whose
    // assistant replies dumped file contents) — ~34k chars ≈ 9k estimated
    // tokens against a 6,144 num_ctx. The task itself is small and sits
    // up front (the protected head), the bulk is summarizable middle, and
    // the recent tail is small — compression CAN reach the budget here,
    // so a still-over-budget dispatch would be a wiring failure, not an
    // incompressibility artifact.
    let filler = "the quick brown newt reads three fifty-kilobyte fixtures\n".repeat(50);
    let mut messages = vec![MemMessage::system("you are a test"), MemMessage::user(TASK)];
    for _ in 0..12 {
        messages.push(MemMessage::assistant(format!("file contents: {filler}")));
        messages.push(MemMessage::user("continue"));
    }

    let ws = gauntlet_workspace();
    let workspace = ws.path().to_string_lossy().to_string();
    let caveats = Caveats::top();
    let uri = server.uri();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let summarizer = canned_summarizer(prompts.clone());
    let mut compress_state = CompressState::new();
    let mut c = ctx(&uri, &messages, &caveats, &workspace);
    // First turn of a fresh session: NO capability-cache numbers, NO
    // token threshold — pre-#282 nothing armed the trigger. The only
    // ceiling in play is the num_ctx the loop itself is about to send.
    c.max_ok_input = None;
    c.safe_context = None;
    c.mid_loop_trim_tokens = None;
    // The expanded always-on catalog plus the exact prompt/card needs
    // ~3.5k tokens by itself. Derive the window from the live catalog:
    // reserve ~1,130 tokens of headroom above it (a catalog-INDEPENDENT
    // figure sized for the compressed head/card/summary/tail) as the input
    // ceiling, then back out the num_ctx that yields it. So the truthful
    // input budget tracks catalog growth while still forcing this ~12k-token
    // full request through compression before its first dispatch. (At
    // today's catalog this reproduces the historical 6,144 num_ctx /
    // 4,915-token ceiling.)
    let input_ceiling = builtin_catalog_tokens(PromptDisposition::Act)
        + prompt_read::response_repository_policy_tokens()
        + 1_130;
    let num_ctx = (input_ceiling * 100).div_ceil(c.input_ceiling_pct as usize) as u32;
    // The actual ceiling the loop derives (`num_ctx_input_ceiling`), reused
    // by the fit assertion below so budget and check stay in lockstep.
    let ceiling = num_ctx as usize * c.input_ceiling_pct as usize / 100;
    c.num_ctx = Some(num_ctx);
    c.summarizer = Some(&*summarizer);
    c.compress_state = Some(&mut compress_state);
    let (reply, _streamed, _usage, _hallu) = chat_complete(c, &mut NoMcp)
        .await
        .expect("the first turn must complete");

    // The turn produced the real answer (visibly degraded, not silently
    // wrong: compression ran and the model answered from the summary).
    assert_eq!(reply, "the marker is GAUNTLET-7f3d9c");

    // Compression fired BEFORE the first dispatch: the summarizer ran,
    // and the VERY FIRST request the backend ever saw already carried
    // the compaction marker. Step 24.4 (#559): this ~34k-char middle now
    // exceeds the per-request cap, so the ONE compression event issues
    // several BOUNDED chunk + reduce summary requests instead of a single
    // truncated one — assert ≥1 (the before-dispatch guarantee is the
    // `first_had_marker` check below), not exactly one.
    assert!(
        !prompts.lock().unwrap().is_empty(),
        "compression ran before the first dispatch (≥1 bounded summary request)"
    );
    let log = log.lock().unwrap();
    let (first_had_marker, first_message_tokens, first_request_tokens) =
        *log.first().expect("at least one request dispatched");
    assert!(
        first_had_marker,
        "B6: the first dispatched request must already be compressed — \
             pre-#282 it went out raw at ~9k tokens"
    );

    // And the COMPLETE request dispatched under the ceiling
    // (`input_ceiling_pct`% of the derived num_ctx). Counting only messages
    // here would hide the catalog cost.
    assert!(
        first_request_tokens <= ceiling,
        "first dispatch must fit the num_ctx input ceiling \
             (got ~{first_request_tokens} est. full-request tokens, \
              including ~{first_message_tokens} message tokens, > {ceiling})"
    );

    // Summarize-don't-discard still holds on the turn-1 path.
    assert!(task_in_marker.load(Ordering::SeqCst), "task survives");
    assert!(summary_in_marker.load(Ordering::SeqCst), "summary present");
    assert!(!old_placeholder.load(Ordering::SeqCst));
}

/// Summarizer endpoint returns 500 → the static marker is dispatched
/// instead and the turn still completes (never aborts).
#[tokio::test]
async fn summarizer_500_degrades_to_static_marker_and_turn_completes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/summarize"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    let log = Arc::new(Mutex::new(Vec::new()));
    let task_in_marker = Arc::new(AtomicBool::new(false));
    let static_in_marker = Arc::new(AtomicBool::new(false));
    let old_placeholder = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(GauntletResponder {
            final_answer: "completed despite summarizer outage".into(),
            log: log.clone(),
            task_in_marker_request: task_in_marker.clone(),
            summary_in_marker_request: static_in_marker.clone(),
            old_placeholder_seen: old_placeholder.clone(),
            static_marker_instead: true,
        })
        .mount(&server)
        .await;

    // A summarizer that really performs the HTTP call — and gets a 500.
    let attempts = Arc::new(AtomicUsize::new(0));
    let summarize_url = format!("{}/summarize", server.uri());
    let attempts_in = attempts.clone();
    let summarizer: Summarizer = Box::new(move |prompt: String| {
        let url = summarize_url.clone();
        let attempts = attempts_in.clone();
        Box::pin(async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            let resp = reqwest::Client::new()
                .post(&url)
                .body(prompt)
                .send()
                .await?;
            if !resp.status().is_success() {
                anyhow::bail!("summarizer endpoint {}", resp.status());
            }
            Ok(resp.text().await?)
        })
    });

    let ws = gauntlet_workspace();
    let workspace = ws.path().to_string_lossy().to_string();
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut compress_state = CompressState::new();
    let mut c = ctx(&uri, &messages, &caveats, &workspace);
    // The complete-request gate now honestly includes the advertised
    // schemas. Derive the threshold as the live catalog weight plus a
    // ~1.8k-token sliver (a catalog-INDEPENDENT offset) that holds the task
    // plus the static fallback marker, so this test keeps isolating a
    // summarizer failure rather than tipping into an irreducible-window
    // refusal as the catalog grows. (Reproduces the historical 5,600 at
    // today's catalog size.)
    c.mid_loop_trim_tokens = Some(
        builtin_catalog_tokens(PromptDisposition::Act)
            + prompt_read::response_repository_policy_tokens()
            + 1_815,
    );
    c.summarizer = Some(&*summarizer);
    c.compress_state = Some(&mut compress_state);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("a summarizer failure must never abort the turn");

    assert_eq!(reply, "completed despite summarizer outage");
    assert!(
        attempts.load(Ordering::SeqCst) >= 1,
        "the summarizer endpoint must have been attempted"
    );
    assert!(
        static_in_marker.load(Ordering::SeqCst),
        "the static fallback marker must reach the model"
    );
    assert!(task_in_marker.load(Ordering::SeqCst), "task still anchored");
    assert!(!old_placeholder.load(Ordering::SeqCst));
}

/// OpenAI-path mirror: the same pipeline serves the second loop — the
/// marker + anchored task reach the post-compression request.
struct OpenAiGauntletResponder {
    final_answer: String,
    task_in_marker_request: Arc<AtomicBool>,
    summary_in_marker_request: Arc<AtomicBool>,
    directive_in_marker_request: Arc<AtomicBool>,
}

impl Respond for OpenAiGauntletResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        if messages_contain(&body, SUMMARY_PREFIX) {
            if messages_contain(&body, TASK) {
                self.task_in_marker_request.store(true, Ordering::SeqCst);
            }
            if messages_contain(&body, CANNED_SUMMARY) {
                self.summary_in_marker_request.store(true, Ordering::SeqCst);
            }
            if messages_contain(&body, compress::CONTINUATION_PREFIX) {
                self.directive_in_marker_request
                    .store(true, Ordering::SeqCst);
            }
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": self.final_answer } }]
            }));
        }
        if body.get("tools").is_some() {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\":\"big.txt\"}" }
                    }]
                }}]
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "cap exit" } }]
            }))
        }
    }
}

#[tokio::test]
async fn openai_loop_compresses_with_the_same_pipeline() {
    let server = MockServer::start().await;
    let task_in_marker = Arc::new(AtomicBool::new(false));
    let summary_in_marker = Arc::new(AtomicBool::new(false));
    let directive_in_marker = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiGauntletResponder {
            final_answer: "openai: marker is GAUNTLET-7f3d9c".into(),
            task_in_marker_request: task_in_marker.clone(),
            summary_in_marker_request: summary_in_marker.clone(),
            directive_in_marker_request: directive_in_marker.clone(),
        })
        .mount(&server)
        .await;

    let ws = gauntlet_workspace();
    let workspace = ws.path().to_string_lossy().to_string();
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let summarizer = canned_summarizer(prompts.clone());
    let mut compress_state = CompressState::new();
    let mut c = ctx(&uri, &messages, &caveats, &workspace);
    c.kind = BackendKind::Openai;
    c.api_key = Some("sk-test");
    c.summarizer = Some(&*summarizer);
    c.compress_state = Some(&mut compress_state);
    // Drive the count trigger: structural pruning shortens read results but
    // cannot reduce message cardinality, so the shared summary stage must
    // run on this wire too.
    c.mid_loop_trim_tokens = None;
    c.mid_loop_trim_threshold = 8;
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("openai loop should succeed");

    assert_eq!(reply, "openai: marker is GAUNTLET-7f3d9c");
    assert!(!prompts.lock().unwrap().is_empty(), "summarizer engaged");
    assert!(task_in_marker.load(Ordering::SeqCst));
    assert!(summary_in_marker.load(Ordering::SeqCst));
    // This compaction fired MID-TURN (tool rounds preceded it), so the
    // real pipeline outcome must also carry the act-now continuation
    // directive to the wire (commit 2's seam, exercised end to end).
    assert!(
        directive_in_marker.load(Ordering::SeqCst),
        "the post-compaction act-now directive must reach the wire"
    );
}

// -----------------------------------------------------------------------
// Multi-compression long-haul regressions (review of PR #267, F1/F2/N3):
// the original suite never exercised a SECOND compression — the gap that
// let the self-poisoning boundary bug through.
// -----------------------------------------------------------------------

/// Per-request long-haul observations: `(dispatched message count,
/// length of the last tool-role message — the freshest result)`.
type HaulLog = Arc<Mutex<Vec<(usize, Option<usize>)>>>;

/// Endless-work responder: each round calls a (hallucinated) write-ish
/// tool and then `read_file` of `path` while tools are offered (the loop
/// runs to its round cap), answering only the cap-exit tools-disabled
/// completion. The non-read-only call keeps the loop's read-only nudge
/// quiet — no intervening user messages, the regime the reviewer's F1
/// traces locked up in (a periodic nudge would hand the boundary a fresh
/// anchor and mask the bug). Logs, per tool-offering request, the
/// dispatched message count and the length of the LAST tool-role message
/// — the freshest result the model is about to read.
struct LongHaulResponder {
    path: &'static str,
    log: HaulLog,
}

impl Respond for LongHaulResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        if body.get("tools").is_some() {
            let empty = Vec::new();
            let msgs = body["messages"].as_array().unwrap_or(&empty);
            let last_tool_len = msgs
                .iter()
                .rev()
                .find(|m| m["role"].as_str() == Some("tool"))
                .and_then(|m| m["content"].as_str())
                .map(|c| c.chars().count());
            self.log.lock().unwrap().push((msgs.len(), last_tool_len));
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": "", "tool_calls": [
                    { "function": { "name": "apply_patch", "arguments": {} } },
                    { "function": { "name": "read_file", "arguments": { "path": self.path } } }
                ]}
            }))
        } else {
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "message": { "content": "long haul done" } }))
        }
    }
}

/// Three prior turns, then the active task — the reviewer's multi-turn
/// shape (the last REAL user message sits deep before the tool rounds).
fn multi_turn_msgs() -> Vec<MemMessage> {
    vec![
        MemMessage::system("you are a test"),
        MemMessage::user("prior turn: inspect the workspace"),
        MemMessage::assistant("inspected — looks healthy"),
        MemMessage::user("prior turn: run the linters"),
        MemMessage::assistant("linters are green"),
        MemMessage::user("prior turn: sketch a fix"),
        MemMessage::assistant("sketched in my head"),
        MemMessage::user(TASK),
    ]
}

/// Leave exactly one estimated token of room for the initial request. This
/// keeps the hard-budget regressions coupled to the live advertised tool
/// catalog rather than a stale numeric snapshot of its schema overhead.
fn initial_request_budget(messages: &[MemMessage], task: &str) -> usize {
    let tools = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    let mut wire_messages: Vec<serde_json::Value> = messages
            .iter()
            .map(|message| {
                serde_json::json!({"role": message.role.as_str(), "content": message.content})
            })
            .collect();
    let receipt = crate::TurnPromptContext::ephemeral_operator(
        "ephemeral-headless",
        task.as_bytes().to_vec(),
        task.as_bytes().to_vec(),
    );
    prompt_read::ensure_active_prompt_card(
        &mut wire_messages,
        prompt_read::PromptReadContext::new(Some(&receipt), task, None),
    );
    estimate_request_tokens(
        &wire_messages,
        Some(&tools),
        crate::tokens::TokenEstimation::default(),
    )
    .saturating_add(1)
}

/// Drive `rounds` tool rounds under count-only compression pressure.
/// Returns `(per-request log, summarizer invocations, reply, latched)`.
async fn run_long_haul(
    mem_messages: Vec<MemMessage>,
    rounds: usize,
    threshold: usize,
    file: &'static str,
    content: &str,
) -> (Vec<(usize, Option<usize>)>, usize, String, bool) {
    let server = MockServer::start().await;
    let log = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(LongHaulResponder {
            path: file,
            log: log.clone(),
        })
        .mount(&server)
        .await;
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join(file), content).unwrap();
    let workspace = ws.path().to_string_lossy().to_string();
    let caveats = Caveats::top();
    let uri = server.uri();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let summarizer = canned_summarizer(prompts.clone());
    let mut compress_state = CompressState::new();
    let mut c = ctx(&uri, &mem_messages, &caveats, &workspace);
    c.max_tool_rounds = rounds;
    c.mid_loop_trim_threshold = threshold;
    c.mid_loop_trim_tokens = None; // count-only: the F1/F2 regime
    c.summarizer = Some(&*summarizer);
    c.compress_state = Some(&mut compress_state);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("the long haul must complete");
    let log = log.lock().unwrap().clone();
    let calls = prompts.lock().unwrap().len();
    (log, calls, reply, compress_state.is_disabled())
}

/// F1 regression (i) — the reviewer's single-turn trace: 4 KB read_file
/// results to round 40 under the count-only trigger. Pre-fix, the second
/// compression anchored on its own summary: the count never shrank again
/// (65 messages at round 40), the summarizer re-fired per round, and the
/// fresh 4 KB result was one-lined before every dispatch from round ~20
/// — the model could never read anything for the rest of the turn.
#[tokio::test]
async fn forty_rounds_single_turn_stay_bounded_with_fresh_results_intact() {
    let line = "the quick brown newt compresses context without discarding it\n";
    let threshold = 15usize;
    let (log, summarizer_calls, reply, latched) =
        run_long_haul(msgs(), 40, threshold, "big.txt", &line.repeat(64)).await;

    assert_eq!(reply, "long haul done");
    assert!(!latched, "count-only pressure must never latch anti-thrash");
    assert_eq!(log.len(), 40, "all 40 tool rounds dispatched");
    for (round, (len, last_tool)) in log.iter().enumerate() {
        assert!(
            *len <= threshold + 6,
            "round {round}: dispatched {len} messages — the count must stay \
                 bounded after every compression (threshold {threshold} + slack)"
        );
        if let Some(n) = last_tool {
            assert!(
                *n > 1_000,
                "round {round}: the fresh tool result was destroyed before \
                     dispatch ({n} chars — a one-liner)"
            );
        }
    }
    assert!(summarizer_calls >= 2, "the long haul compresses repeatedly");
    assert!(
        summarizer_calls <= 16,
        "summarizer invocations must be bounded, not per-round \
             (got {summarizer_calls} in 40 rounds)"
    );
    let max_len = log.iter().map(|(l, _)| *l).max().unwrap();
    println!(
        "forty-round trace: max dispatched len {max_len}, \
             summarizer calls {summarizer_calls}"
    );
}

/// F1 regression (ii) — the reviewer's multi-turn trace: 3 prior turns,
/// then 30 tool rounds. Pre-fix the regime locked in at round ~9 (the
/// anchor pinned the current task, the middle shrank to the previous
/// summary alone, nothing could ever shrink) and the summarizer re-ran
/// almost every round (71 invocations in 80 rounds).
#[tokio::test]
async fn thirty_rounds_multi_turn_stay_bounded_with_fresh_results_intact() {
    let line = "the quick brown newt compresses context without discarding it\n";
    let threshold = 15usize;
    let (log, summarizer_calls, reply, latched) = run_long_haul(
        multi_turn_msgs(),
        30,
        threshold,
        "big.txt",
        &line.repeat(64),
    )
    .await;

    assert_eq!(reply, "long haul done");
    assert!(!latched, "count-only pressure must never latch anti-thrash");
    assert_eq!(log.len(), 30, "all 30 tool rounds dispatched");
    for (round, (len, last_tool)) in log.iter().enumerate() {
        assert!(
            *len <= threshold + 6,
            "round {round}: dispatched {len} messages — bounded"
        );
        if let Some(n) = last_tool {
            assert!(
                *n > 1_000,
                "round {round}: fresh tool result destroyed pre-dispatch ({n} chars)"
            );
        }
    }
    assert!(summarizer_calls >= 2);
    assert!(
        summarizer_calls <= 14,
        "summarizer invocations must be bounded, not per-round \
             (got {summarizer_calls} in 30 rounds)"
    );
    let max_len = log.iter().map(|(l, _)| *l).max().unwrap();
    println!(
        "thirty-round multi-turn trace: max dispatched len {max_len}, \
             summarizer calls {summarizer_calls}"
    );
}

/// F2 regression — the reviewer's 600-char-results multi-turn shape:
/// count-only compressions whose per-pass reclaim is small must neither
/// latch anti-thrash (silently killing the VRAM guard) nor escalate to
/// the Refused bail — pre-fix this errored the whole turn at round 25
/// with "context exceeds the model's input budget" while the actual
/// context was ~3-5k tokens and NO token threshold was configured.
#[tokio::test]
async fn count_only_low_reclaim_never_latches_or_bails() {
    let (log, _calls, reply, latched) =
        run_long_haul(multi_turn_msgs(), 30, 15, "small.txt", &"x".repeat(600)).await;
    assert_eq!(reply, "long haul done", "the turn must complete — no bail");
    assert!(
        !latched,
        "count-only passes must never latch the disable switch"
    );
    assert_eq!(log.len(), 30, "all 30 rounds ran (no Refused early-exit)");
}

// -----------------------------------------------------------------------
// Trailing-group long-hauls (#270 / #285): the read-only-nudge regime the
// #267 re-verifier flagged as untested, and the oversized-single-round
// residual #284's gauntlet measured.
// -----------------------------------------------------------------------

/// Per-request observations for the nudged haul: `(message count, nudge
/// text present, per-result content lengths of the trailing tool group)`.
type NudgedLog = Arc<Mutex<Vec<(usize, bool, Vec<usize>)>>>;

/// Read-only-only responder: every round reads big1 + big2 + small (three
/// read-only calls, no writes), so the loop's read-only nudge fires every
/// few rounds — the regime #270's probe ran in. The #267 long-haul
/// responder deliberately added a write-ish call to keep that nudge quiet
/// and hand the boundary no fresh anchors; this one deliberately does the
/// opposite. Logs, per tool-offering request, the content length of every
/// tool result in the trailing group (everything after the last
/// assistant-with-`tool_calls`).
struct NudgedHaulResponder {
    log: NudgedLog,
}

impl Respond for NudgedHaulResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        if body.get("tools").is_some() {
            let empty = Vec::new();
            let msgs = body["messages"].as_array().unwrap_or(&empty);
            let group_start = msgs
                .iter()
                .rposition(|m| m["tool_calls"].as_array().is_some_and(|t| !t.is_empty()))
                .map(|i| i + 1)
                .unwrap_or(msgs.len());
            let group_lens: Vec<usize> = msgs[group_start..]
                .iter()
                .filter(|m| m["role"].as_str() == Some("tool"))
                .filter_map(|m| m["content"].as_str())
                .map(|c| c.chars().count())
                .collect();
            let nudged = messages_contain(&body, "read-only rounds so far");
            self.log
                .lock()
                .unwrap()
                .push((msgs.len(), nudged, group_lens));
            // `role` present like a real Ollama reply — the loop appends
            // this object verbatim and the prune pairing reads the role.
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "role": "assistant", "content": "", "tool_calls": [
                    { "function": { "name": "read_file", "arguments": { "path": "big1.txt" } } },
                    { "function": { "name": "read_file", "arguments": { "path": "big2.txt" } } },
                    { "function": { "name": "read_file", "arguments": { "path": "small.txt" } } }
                ]}
            }))
        } else {
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "message": { "content": "nudged haul done" } }))
        }
    }
}

/// #270 e2e — the test the #267 re-verifier said was missing: a
/// nudge-active long session (read-only rounds only, so the loop injects
/// its stop-exploring user message right before the compression call
/// site every few rounds) under a hard token trigger. Pre-fix, the
/// nudge zeroed the trailing `role == "tool"` count, `keep_last`
/// floored at 2, and BOTH big fresh results were one-lined pre-dispatch
/// on every nudge round. Post-fix the group derives from the assistant
/// turn that issued the calls: the newest member is always whole and
/// the middle member survives every round (#285's within-group reclaim
/// may one-line only the oldest, oldest-first, and only while over
/// budget).
#[tokio::test]
async fn nudged_long_haul_keeps_fresh_group_results_intact() {
    let server = MockServer::start().await;
    let log: NudgedLog = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(NudgedHaulResponder { log: log.clone() })
        .mount(&server)
        .await;
    // Distinct contents per file: identical results would engage the
    // dedupe pass, which is not what this test pins.
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(
        ws.path().join("big1.txt"),
        "the first big fixture keeps unseen results intact\n".repeat(200),
    )
    .unwrap();
    std::fs::write(
        ws.path().join("big2.txt"),
        "the second big fixture must survive every nudge round\n".repeat(200),
    )
    .unwrap();
    std::fs::write(
        ws.path().join("small.txt"),
        "the small newest fixture stays whole\n".repeat(5),
    )
    .unwrap();
    let workspace = ws.path().to_string_lossy().to_string();
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let summarizer = canned_summarizer(prompts.clone());
    let mut compress_state = CompressState::new();
    let mut c = ctx(&uri, &messages, &caveats, &workspace);
    c.max_tool_rounds = 12;
    // Derive the trigger from the live Always-on catalog (#1387 grew it)
    // plus a catalog-independent headroom for one complete fresh result
    // group — same shape as the other compression-loop fixtures.
    c.mid_loop_trim_tokens = Some(
        builtin_catalog_tokens(PromptDisposition::Act)
            + prompt_read::response_repository_policy_tokens()
            + 4_600,
    );
    c.summarizer = Some(&*summarizer);
    c.compress_state = Some(&mut compress_state);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("the nudged haul must complete");

    assert_eq!(reply, "nudged haul done");
    assert!(
        !compress_state.is_disabled(),
        "real reclaims every round must never latch anti-thrash"
    );
    let log = log.lock().unwrap();
    for (round, entry) in log.iter().enumerate() {
        println!("nudged haul round {round}: {entry:?}");
    }
    assert_eq!(log.len(), 12, "all 12 tool rounds dispatched");
    let nudged_rounds = log.iter().filter(|(_, n, _)| *n).count();
    assert!(
        nudged_rounds >= 2,
        "the read-only nudge regime must actually be active \
             (got {nudged_rounds} nudged requests)"
    );
    for (round, (len, nudged, group_lens)) in log.iter().enumerate() {
        if group_lens.is_empty() {
            continue; // round 0: no tool group yet
        }
        assert_eq!(group_lens.len(), 3, "round {round}: pairing intact");
        // The newest member is ALWAYS whole.
        assert!(
            group_lens[2] > 150,
            "round {round} (nudged={nudged}): the newest fresh result \
                 was destroyed pre-dispatch ({} chars)",
            group_lens[2]
        );
        // The middle member survives too: within-group reclaim is
        // oldest-first and stops at the budget — pre-#270 every nudge
        // round one-lined it ({len} msgs dispatched).
        assert!(
            group_lens[1] > 1_000,
            "round {round} (nudged={nudged}, {len} msgs): the middle \
                 fresh result was destroyed pre-dispatch ({} chars)",
            group_lens[1]
        );
    }
    let max_tokens = log.iter().map(|(l, _, _)| *l).max().unwrap();
    println!(
        "nudged haul trace: {nudged_rounds} nudged rounds, \
             max dispatched len {max_tokens}, group lens e.g. {:?}",
        log.last().unwrap().2
    );
}

/// Per-request observations for the oversized-round haul: `(est message
/// tokens, a one-lined, b one-lined, c payload intact, task present)`.
type OversizedLog = Arc<Mutex<Vec<(usize, bool, bool, bool, bool)>>>;

/// One round reads three files whose results TOGETHER exceed the model
/// window; answers as soon as the dispatch shows a.txt one-lined.
struct OversizedRoundResponder {
    log: OversizedLog,
}

impl Respond for OversizedRoundResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let a_onelined = messages_contain(&body, "[read_file] read 'a.txt'");
        if !body["stream"].as_bool().unwrap_or(false) {
            self.log.lock().unwrap().push((
                body_request_tokens(&body),
                a_onelined,
                messages_contain(&body, "[read_file] read 'b.txt'"),
                messages_contain(&body, "NEWEST-PAYLOAD-C-INTACT"),
                messages_contain(&body, TASK),
            ));
        }
        if a_onelined {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": "the three files are summarized" }
            }));
        }
        if body.get("tools").is_some() {
            // `role` present like a real Ollama reply — the prune
            // pairing needs it to name the file in each one-liner.
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "role": "assistant", "content": "", "tool_calls": [
                    { "function": { "name": "read_file", "arguments": { "path": "a.txt" } } },
                    { "function": { "name": "read_file", "arguments": { "path": "b.txt" } } },
                    { "function": { "name": "read_file", "arguments": { "path": "c.txt" } } }
                ]}
            }))
        } else {
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "message": { "content": "cap exit" } }))
        }
    }
}

/// #285 e2e — the B6 residual measured in #284's gauntlet: ONE round's
/// tool group alone exceeds the `num_ctx` ceiling (the only budget in
/// play on a fresh capability cache, #282/#284 wiring untouched).
/// Pre-fix the F1c protection made the group unreclaimable: compression
/// honestly reported "still over budget" and the dispatch shipped
/// over-window into a silent backend truncation. Post-fix the dispatch
/// fits the ceiling: a.txt / b.txt one-lined (each naming its file for
/// re-read), c.txt — the newest — intact, the task still present, and
/// the model returns the real answer.
#[tokio::test]
async fn oversized_single_round_dispatches_within_the_window() {
    let server = MockServer::start().await;
    let log: OversizedLog = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OversizedRoundResponder { log: log.clone() })
        .mount(&server)
        .await;
    // Three ~7 KB results plus Always-on schemas must exceed the input
    // ceiling before reclaim (B6's shape). Derive num_ctx from the live
    // catalog (#1387 grew Always-on tools) plus fixed headroom so the
    // relative property stays stable as the catalog changes.
    // Distinct contents per file: identical results would engage the
    // dedupe pass, which is not what this test pins.
    let catalog = builtin_catalog_tokens(PromptDisposition::Act);
    // ~3.2k tokens of message/result headroom after reclaim (matches the
    // pre-#1387 6,553 − ~3.4k catalog gap).
    let input_ceiling = catalog + prompt_read::response_repository_policy_tokens() + 3_200;
    let num_ctx = ((input_ceiling as f64) / 0.8).ceil() as u32;
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(
        ws.path().join("a.txt"),
        "fixture a arrives first in the one giant trailing group\n".repeat(125),
    )
    .unwrap();
    std::fs::write(
        ws.path().join("b.txt"),
        "fixture b arrives second and is older than the newest\n".repeat(130),
    )
    .unwrap();
    std::fs::write(
        ws.path().join("c.txt"),
        format!(
            "{}NEWEST-PAYLOAD-C-INTACT\n",
            "fixture c arrives last and must reach the model whole\n".repeat(130)
        ),
    )
    .unwrap();
    let workspace = ws.path().to_string_lossy().to_string();
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let summarizer = canned_summarizer(prompts.clone());
    let mut compress_state = CompressState::new();
    let mut c = ctx(&uri, &messages, &caveats, &workspace);
    // Fresh capability cache, no token threshold: the num_ctx the loop
    // itself sends is the only ceiling (#284's regime, untouched).
    c.max_ok_input = None;
    c.safe_context = None;
    c.mid_loop_trim_tokens = None;
    c.num_ctx = Some(num_ctx);
    c.summarizer = Some(&*summarizer);
    c.compress_state = Some(&mut compress_state);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("the oversized round must complete");

    let log = log.lock().unwrap();
    for (i, entry) in log.iter().enumerate() {
        println!("oversized round request {i}: {entry:?}");
    }
    // Never a silent wrong answer: the model answered from real data.
    assert_eq!(reply, "the three files are summarized");

    // THE #285 property: no dispatch ships over the window.
    for (i, &(tokens, ..)) in log.iter().enumerate() {
        assert!(
            tokens <= input_ceiling,
            "request {i} dispatched over the window: ~{tokens} est. \
                 full-request tokens > {input_ceiling}"
        );
    }

    let (tokens, _, b_onelined, c_intact, task_present) = *log
        .iter()
        .find(|(_, a, ..)| *a)
        .expect("a dispatch with a.txt one-lined must have happened");
    assert!(
        b_onelined,
        "older members one-lined in order: b.txt one-liner missing"
    );
    assert!(
        c_intact,
        "#285: the NEWEST result must reach the model whole"
    );
    assert!(task_present, "the task survives the within-group reclaim");
    assert!(
        tokens <= input_ceiling,
        "#285: the reclaimed dispatch must fit the window \
             (got ~{tokens} est. full-request tokens > {input_ceiling})"
    );
    println!(
        "#285 e2e trace: reclaimed dispatch ~{tokens} est. tokens \
             (full-request ceiling {input_ceiling}, num_ctx {num_ctx}), \
             a/b one-lined, c intact"
    );
}

/// A backend-reported prompt count can be lower than Newt's calibrated
/// whole-request estimate. The tracker anchors the next round on that real
/// count, but the authoritative preflight prices the complete request from
/// scratch. Before this regression, that disagreement could skip the
/// compression trigger and then fail at preflight even though older history
/// was reclaimable. The loop must compact and dispatch the healed request.
struct UnderreportedAnchorResponder {
    log: Arc<Mutex<Vec<(bool, usize)>>>,
}

impl Respond for UnderreportedAnchorResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let has_marker = messages_contain(&body, SUMMARY_PREFIX);
        if !body["stream"].as_bool().unwrap_or(false) {
            self.log
                .lock()
                .unwrap()
                .push((has_marker, body_request_tokens(&body)));
        }
        if has_marker {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": "recovered after exact-request compaction" },
                "prompt_eval_count": 100,
                "eval_count": 5
            }));
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": { "content": "", "tool_calls": [{
                "function": { "name": "read_file", "arguments": { "path": "big.txt" } }
            }]},
            // Deliberately below Newt's full-request estimate. This grounds
            // the production seam: the anchor and preflight currencies are
            // both valid observations, but only the latter controls dispatch.
            "prompt_eval_count": 1,
            "eval_count": 1
        }))
    }
}

/// The real temp-file read grounds the mocked backend call: the healed
/// request contains actual `read_file` output, not a hand-built tool result.
#[tokio::test]
async fn exact_request_pressure_self_compacts_after_tracker_underreport() {
    let server = MockServer::start().await;
    let log = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(UnderreportedAnchorResponder { log: log.clone() })
        .mount(&server)
        .await;

    let ws = gauntlet_workspace();
    let workspace = ws.path().to_string_lossy().to_string();
    // A large OLD assistant turn is reclaimable; the active task and fresh
    // read_file result remain protected. The initial request fits by one
    // token, while the follow-up requires compaction.
    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("summarize this old investigation later"),
        MemMessage::assistant("old reclaimable evidence\n".repeat(600)),
        MemMessage::user(TASK),
    ];
    let budget = initial_request_budget(&messages, TASK);
    let caveats = Caveats::top();
    let uri = server.uri();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let summarizer = canned_summarizer(prompts.clone());
    let mut compress_state = CompressState::new();
    let mut c = ctx(&uri, &messages, &caveats, &workspace);
    c.mid_loop_trim_tokens = Some(budget);
    c.summarizer = Some(&*summarizer);
    c.compress_state = Some(&mut compress_state);

    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("exact-request pressure should compact and retry quietly");

    assert_eq!(reply, "recovered after exact-request compaction");
    assert!(
        !prompts.lock().unwrap().is_empty(),
        "self-healing must run the real compression pipeline"
    );
    let log = log.lock().unwrap();
    assert_eq!(log.first().map(|entry| entry.0), Some(false));
    assert!(
        log.iter()
            .skip(1)
            .any(|(marker, tokens)| *marker && *tokens <= budget),
        "a compacted request must be dispatched within the authoritative budget: {log:?}"
    );
}

/// N3 — the loop-level hard-budget refusal path end-to-end. The older
/// regression intentionally dispatched repeated known-over-budget
/// requests until anti-thrash latched. The authoritative full-request
/// gate now supersedes that unsafe route: after one valid tool round, an
/// incompressible follow-up is refused before its wire dispatch.
#[tokio::test]
async fn hard_budget_thrash_latches_then_bails_with_named_error() {
    let server = MockServer::start().await;
    let log = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(LongHaulResponder {
            path: "tiny.txt",
            log: log.clone(),
        })
        .mount(&server)
        .await;
    let ws = tempfile::TempDir::new().unwrap();
    // The protected prompt + catalog fits the configured budget, so the
    // first request is valid. The newest tool result does not fit in the
    // remaining message budget and cannot be discarded.
    std::fs::write(ws.path().join("tiny.txt"), "ok").unwrap();
    let workspace = ws.path().to_string_lossy().to_string();
    // Incompressible: the system prompt dominates and no structural
    // pass or boundary can reduce it.
    let messages = vec![
        MemMessage::system(format!("you are a test. {}", "rule. ".repeat(7_000))),
        MemMessage::user(TASK),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut compress_state = CompressState::new();
    let mut c = ctx(&uri, &messages, &caveats, &workspace);
    c.mid_loop_trim_tokens = Some(initial_request_budget(&messages, TASK));
    c.compress_state = Some(&mut compress_state);
    let err = chat_complete(c, &mut NoMcp)
        .await
        .expect_err("the first known-over-budget follow-up must refuse the send");
    let msg = err.to_string();
    assert!(msg.contains("complete inference request needs"), "{msg}");
    assert!(msg.contains("tool results were not truncated"), "{msg}");
    assert!(
        !compress_state.is_disabled(),
        "preflight refuses before thrashing"
    );
    assert_eq!(
        log.lock().unwrap().len(),
        1,
        "no known-over-budget follow-up may reach the wire"
    );
}

/// Step 20.3 — the loop-level fail-open path (the gpt-4.1 bug). Same
/// incompressible over-budget shape as the bail test, but the budget rests
/// on the proven-good high-water mark ALONE (`max_ok_input`, no
/// `safe_context`, no `num_ctx`, no token threshold) — the cloud /
/// no-`/api/show` case. Anti-thrash still latches, but the latched
/// over-budget rounds must NOT refuse: refusing here is the death spiral
/// the user hit. The loop keeps dispatching (fails open) and never bails
/// with the named error.
#[tokio::test]
async fn lone_hwm_budget_fails_open_and_does_not_bail() {
    let server = MockServer::start().await;
    let log = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(LongHaulResponder {
            path: "tiny.txt",
            log: log.clone(),
        })
        .mount(&server)
        .await;
    let ws = tempfile::TempDir::new().unwrap();
    std::fs::write(ws.path().join("tiny.txt"), "ok").unwrap();
    let workspace = ws.path().to_string_lossy().to_string();
    let messages = vec![
        MemMessage::system(format!("you are a test. {}", "rule. ".repeat(700))),
        MemMessage::user(TASK),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut compress_state = CompressState::new();
    let mut c = ctx(&uri, &messages, &caveats, &workspace);
    // The cloud shape: a starved proven-good HWM and NOTHING authoritative.
    c.mid_loop_trim_tokens = None;
    c.max_ok_input = Some(50); // largest prompt "seen" — a floor, not a cap
    c.safe_context = None; // no /api/show seed
    c.num_ctx = None; // no per-request window ceiling
    c.compress_state = Some(&mut compress_state);
    let result = chat_complete(c, &mut NoMcp).await;
    // The session must NOT die on the budget bail — it fails open.
    if let Err(e) = &result {
        let msg = e.to_string();
        assert!(
            !msg.contains("exceeds the model's input budget"),
            "a lone-HWM budget must never refuse the send: {msg}"
        );
    }
    assert!(
        compress_state.is_disabled(),
        "anti-thrash still latches on the poor passes"
    );
    assert!(
        log.lock().unwrap().len() > 3,
        "the loop kept dispatching past the latch instead of bailing"
    );
}
