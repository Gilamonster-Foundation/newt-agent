//! BAT (#1265): the "10 largest Rust files" session, replayed end-to-end.
//!
//! EPIC #1257's acceptance gate. Each Cluster A/B story pins its own behavior
//! at the unit tier; the diagnosed `ornith:35b` failure was the COMPOSITION —
//! classification → tool gate → capability gap → forensics — so this scenario
//! replays the whole turn against a simulated integration environment (scripted
//! backend via wiremock, a tempdir workspace per the BAT tier in `CLAUDE.md`)
//! and asserts the double-bind is gone and STAYS gone:
//!
//! 1. the prompt classifies by CONTENT (Research via "largest", #1260 — never
//!    the trailing-`?` cliff);
//! 2. the evidence toolset can answer (`find` with `sort=size`+`show_size`,
//!    #1258);
//! 3. the escalation path is formal (`request_user_input`, #1259) — not
//!    penalized narration;
//! 4. the forensics tell the truth: `TurnEndReason::Completed`,
//!    `hallucinations == 0`, and a footer with NO ⚠ (#1261, #1262).
//!
//! Fast + fully simulated (scripted model, no live inference), so it runs in
//! the per-PR suite.

use super::*;
use crate::caveats::Caveats;
use crate::{BackendKind, MemMessage};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// The diagnosed session's operator prompt, verbatim.
const PROMPT: &str = "What are the 10 largest Rust files in this workspace?";

fn msgs() -> Vec<MemMessage> {
    vec![
        MemMessage::system("you are a test"),
        MemMessage::user(PROMPT),
    ]
}

/// The scripted backend: serves `choices[0].message` per request in order,
/// repeating the last entry (the http_loop `ScriptedOpenAi` shape, kept local
/// so the BAT module is self-contained).
struct ScriptedModel {
    round: Arc<AtomicUsize>,
    script: Vec<serde_json::Value>,
}
impl Respond for ScriptedModel {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let i = self.round.fetch_add(1, Ordering::SeqCst);
        let msg = self
            .script
            .get(i)
            .or_else(|| self.script.last())
            .cloned()
            .unwrap_or_else(|| serde_json::json!({ "content": "final." }));
        ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({ "choices": [{ "message": msg }] }))
    }
}

/// A simulated workspace: Rust files with KNOWN, distinct sizes so the size
/// ordering is deterministic. (Real tempdir fs — sanctioned in the BAT tier;
/// the model and every external system stay mocked.)
fn simulated_workspace() -> tempfile::TempDir {
    let ws = tempfile::TempDir::new().expect("workspace");
    for (name, bytes) in [("small.rs", 10), ("large.rs", 3_000), ("mid.rs", 300)] {
        std::fs::write(ws.path().join(name), vec![b'x'; bytes]).expect("seed file");
    }
    ws
}

/// Run the scripted turn under the REAL intake classification for the prompt
/// (composition, not a hardcoded disposition) and return
/// `(reply, hallucinations, end_reason, wire_bodies)`.
async fn run_scenario(
    workspace: &std::path::Path,
    script: Vec<serde_json::Value>,
) -> (String, u32, Option<crate::TurnEndReason>, String) {
    let intake = PromptIntake::analyze(PROMPT);
    // #1260: content classifies (the "largest" evidence needle) — the `?`
    // cliff no longer decides. Research keeps the bounded evidence loop.
    assert_eq!(
        intake.disposition(),
        PromptDisposition::Research,
        "the canonical prompt must classify Research by content"
    );

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedModel {
            round: Arc::new(AtomicUsize::new(0)),
            script,
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let ws = workspace.to_string_lossy().into_owned();
    let mut end_reason: Option<crate::TurnEndReason> = None;
    let mut c = ChatCtx {
        url: &uri,
        model: "test-model",
        kind: BackendKind::Openai,
        api_key: None,
        messages: &messages,
        task: PROMPT,
        workspace: &ws,
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
        caveats: &caveats,
        persona_tools: None,
        max_tool_rounds: 8,
        narration_nudge_cap: 1,
        action_nudges: true,
        prompt_disposition: intake.disposition(),
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
    };
    c.end_reason = Some(&mut end_reason);
    let (reply, _streamed, _usage, hallucinations) = chat_complete(c, &mut NoMcp)
        .await
        .expect("the replayed turn completes");

    let bodies = server
        .received_requests()
        .await
        .expect("recorded")
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect::<Vec<_>>()
        .join("\n---\n");
    (reply, hallucinations, end_reason, bodies)
}

/// The whole point of the epic, asserted in one place: the footer renders with
/// NO ⚠ for this turn.
fn assert_clean_footer(hallucinations: u32, end_reason: Option<crate::TurnEndReason>) {
    assert_eq!(
        end_reason,
        Some(crate::TurnEndReason::Completed),
        "the turn is a completion, never a narration anomaly (#1261)"
    );
    assert_eq!(hallucinations, 0, "no false hallucination counts (#1262)");
    let metrics = crate::TurnMetrics {
        hallucinations,
        end_reason,
        ..Default::default()
    };
    assert!(
        !metrics.display_line().contains('⚠'),
        "the footer must be clean: {}",
        metrics.display_line()
    );
}

/// Flow 1 — the capable path: the model answers with the sized `find` the
/// evidence toolset now carries (#1258). Clean footer.
#[tokio::test]
async fn largest_files_question_answers_with_sized_find_and_clean_footer() {
    let ws = simulated_workspace();
    let (reply, hallucinations, end_reason, wire) = run_scenario(
        ws.path(),
        vec![
            serde_json::json!({
                "content": null,
                "tool_calls": [{
                    "id": "c1", "type": "function",
                    "function": { "name": "find",
                        "arguments": "{\"path\":\".\",\"name\":\"*.rs\",\"type\":\"f\",\"sort\":\"size\",\"show_size\":true,\"max_results\":10}" }
                }]
            }),
            serde_json::json!({ "content":
                "The largest Rust files are large.rs (3000 bytes), mid.rs (300) and small.rs (10)." }),
        ],
    )
    .await;

    assert!(
        reply.contains("largest Rust files"),
        "final answer returned: {reply}"
    );
    // The evidence turn ANSWERED the size question through `find` — byte sizes,
    // descending — no shell, no pipeline, no box-in.
    assert!(
        wire.contains("3000\\tlarge.rs") || wire.contains("3000\tlarge.rs"),
        "the sized find result must reach the model, size-first: {wire}"
    );
    let large = wire.find("3000").expect("largest present");
    let mid = wire
        .find("300\\tmid.rs")
        .or_else(|| wire.find("300\tmid.rs"));
    assert!(
        mid.is_some_and(|m| large < m),
        "descending size order on the wire"
    );
    assert_clean_footer(hallucinations, end_reason);
}

/// Flow 2 — the boxed-in path made honest: the model first tries the `du`
/// pipeline (disposition-denied — NOT miscounted, #1262), then formally asks
/// the human (#1259) instead of being trapped into penalized narration. The
/// footer still renders clean (#1261).
#[tokio::test]
async fn largest_files_question_pipeline_denied_then_escalates_cleanly() {
    let ws = simulated_workspace();
    let (reply, hallucinations, end_reason, wire) = run_scenario(
        ws.path(),
        vec![
            serde_json::json!({
                "content": null,
                "tool_calls": [{
                    "id": "c1", "type": "function",
                    "function": { "name": "run_command",
                        "arguments": "{\"command\":\"find . -name \\\"*.rs\\\" -type f -print0 | xargs -0 du -k | sort -rn | head 20\"}" }
                }]
            }),
            serde_json::json!({
                "content": null,
                "tool_calls": [{
                    "id": "c2", "type": "function",
                    "function": { "name": "request_user_input",
                        "arguments": "{\"question\":\"May I run the du pipeline, or should I use the built-in find?\"}" }
                }]
            }),
            serde_json::json!({ "content": "Proceeding with the built-in find as suggested." }),
        ],
    )
    .await;

    assert!(
        reply.contains("Proceeding"),
        "final answer returned: {reply}"
    );
    // The pipeline was DISPOSITION-refused (an honest gate), never hijacked as
    // a misdirected embedded-find (#1262 kept hallucinations at zero below).
    assert!(
        wire.contains("Tool `run_command` is unavailable"),
        "the evidence turn refuses the shell honestly: {wire}"
    );
    // The formal escalation dispatched (#1259): headless => the recoverable
    // no-human message — never the disposition refusal, never a hang.
    assert!(
        wire.contains("no human available this session"),
        "request_user_input must dispatch in the evidence turn: {wire}"
    );
    assert!(
        !wire.contains("Tool `request_user_input` is unavailable"),
        "the escalation must not be disposition-refused"
    );
    assert_clean_footer(hallucinations, end_reason);
}
