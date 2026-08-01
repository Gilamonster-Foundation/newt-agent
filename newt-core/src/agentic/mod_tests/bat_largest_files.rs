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
//! Flow 1b / 2b lock the **line-count** sibling (live-validated against
//! Nemotron 2026-07-25): "highest line counts" classifies Research, is answered
//! by `find` `sort=lines`+`show_lines` (NOT a bytesize fallback — the fixture
//! inverts byte vs line order so a size sort would fail the assertion), and a
//! `wc -l` shell reach is disposition-denied the same way `du` is for bytes.
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

/// #1387 sibling: the line-count regression prompt. Same double-bind shape as
/// #1257 (an evidence question the read-only turn must answer without shell) —
/// here the metric is line count, answered by `find` sort=lines/show_lines, not
/// a bytesize fallback (which the operator classes a failure).
const LINE_COUNT_PROMPT: &str =
    "show me the 10 code files with the highest line counts in this repository?";

/// The operator's immediate follow-up, verbatim: explicit language + explicit
/// table presentation. This is a new prompt, not a retry, so the protected
/// comprehension card must independently preserve both constraints.
const RUST_TABLE_PROMPT: &str =
    "can you give me a table of the rust files with the longest line counts instead?";

/// A repository-understanding request with no inventory metric, ranking word,
/// file extension, or requested presentation shape. It proves the policy is a
/// standing harness default rather than a repair for the observed prompts.
const GENERAL_REPOSITORY_PROMPT: &str = "analyze how authentication works in this repository";

fn msgs_for(prompt: &str) -> Vec<MemMessage> {
    vec![
        MemMessage::system("you are a test"),
        MemMessage::user(prompt),
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

/// A simulated workspace whose files have KNOWN, distinct LINE counts that are
/// deliberately INVERTED vs byte size — so a bytesize fallback would order
/// differently and fail the assertion. (Real tempdir fs — sanctioned in the
/// BAT tier; the model and every external system stay mocked.)
///
/// | file     | lines | bytes (approx) | line-rank | byte-rank |
/// |----------|------:|---------------:|----------:|----------:|
/// | tall.rs  |   120 |            240 |         1 |         2 |
/// | mid.rs   |    40 |             80 |         2 |         3 |
/// | fat.rs   |     2 |           5001 |         3 |         1 |
fn simulated_line_workspace() -> tempfile::TempDir {
    let ws = tempfile::TempDir::new().expect("workspace");
    // Many short lines → high line count, low bytes.
    std::fs::write(ws.path().join("tall.rs"), "x\n".repeat(120)).expect("tall");
    std::fs::write(ws.path().join("mid.rs"), "x\n".repeat(40)).expect("mid");
    // Few long lines → low line count, high bytes (the bytesize trap).
    let fat = format!("{}\n\n", "Y".repeat(5000));
    std::fs::write(ws.path().join("fat.rs"), fat).expect("fat");
    // Docs / lockfile traps: MORE lines than the Rust sources. Without
    // `code: true` these would dominate a bare line-count ranking — the
    // 2026-07-26 regression class (AGENTS.md / Cargo.lock as "code files").
    std::fs::write(ws.path().join("AGENTS.md"), "d\n".repeat(500)).expect("agents md");
    std::fs::write(ws.path().join("Cargo.lock"), "l\n".repeat(400)).expect("lock");
    ws
}

/// Run the scripted turn under the REAL intake classification for the prompt
/// (composition, not a hardcoded disposition) and return
/// `(reply, hallucinations, end_reason, wire_bodies)`.
async fn run_scenario(
    workspace: &std::path::Path,
    script: Vec<serde_json::Value>,
) -> (String, u32, Option<crate::TurnEndReason>, String) {
    run_scenario_for(PROMPT, workspace, script).await
}

/// As [`run_scenario`], for an arbitrary evidence `prompt`. Both the byte-size
/// (#1257) and line-count (#1387) prompts are Research by content, so the
/// classification invariant is asserted here for whichever prompt is replayed.
async fn run_scenario_for(
    prompt: &str,
    workspace: &std::path::Path,
    script: Vec<serde_json::Value>,
) -> (String, u32, Option<crate::TurnEndReason>, String) {
    let intake = PromptIntake::analyze(prompt);
    // #1260/#1387: content classifies (the "largest"/"line count" evidence
    // needles) — the `?` cliff no longer decides. Research keeps the bounded
    // evidence loop.
    assert_eq!(
        intake.disposition(),
        PromptDisposition::Research,
        "the evidence prompt must classify Research by content"
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

    let messages = msgs_for(prompt);
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
        task: prompt,
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
        nav: None,
        exposure: Default::default(),
        experience_store: None,
        step_ledger: None,
        caveats: &caveats,
        persona_tools: None,
        cognition: None,
        chat_completions_capability: Default::default(),
        reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
        max_tool_rounds: 8,
        narration_nudge_cap: 1,
        action_nudges: true,
        prompt_disposition: intake.disposition(),
        prompt_intake: Some(&intake),
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
        solve_obs: None,
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
        operating_mode_control: None,
        plan_mode_control: None,
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

/// Flow 1b (#1387) — the line-count sibling of Flow 1: the "highest line counts"
/// prompt classifies Research by content and is ANSWERED read-only by `find`
/// with `sort=lines`+`show_lines`. No `wc -l`, no shell, and no bytesize
/// fallback (which the operator classes a failure). The fixture inverts byte vs
/// line order so a size-sort would put `fat.rs` first — locking that the metric
/// is truly lines. Clean footer.
#[tokio::test]
async fn line_count_question_answers_with_lined_find_and_clean_footer() {
    let ws = simulated_line_workspace();
    let (reply, hallucinations, end_reason, wire) = run_scenario_for(
        LINE_COUNT_PROMPT,
        ws.path(),
        vec![
            serde_json::json!({
                "content": null,
                "tool_calls": [{
                    "id": "c1", "type": "function",
                    "function": { "name": "find",
                        "arguments": "{\"path\":\".\",\"category\":\"source\",\"type\":\"f\",\"sort\":\"lines\",\"show_lines\":true,\"max_results\":10}" }
                }]
            }),
            serde_json::json!({ "content":
                "| File | line count |\n|---|---:|\n| `tall.rs` | 120 |\n| `mid.rs` | 40 |\n| `fat.rs` | 2 |" }),
        ],
    )
    .await;

    assert!(
        reply.contains("| Path | Lines |") || reply.contains("tall.rs"),
        "final answer should be a GFM table (or at least name the code files): {reply}"
    );
    // The Research turn must teach `find` the line-count measure + code filter
    // on the wire — if the schema loses `sort=lines`/`show_lines`/`code`, the
    // live model cannot discover the capable path (2026-07-25/26 regressions).
    assert!(
        wire.contains("\"show_lines\"") && wire.contains("\"lines\""),
        "Research-advertised find schema must teach show_lines + sort=lines: {wire}"
    );
    assert!(
        wire.contains("\\\"category\\\":\\\"source\\\"")
            || wire.contains("\"category\":\"source\""),
        "`code files` must use the harness source category, not an unfiltered walk: {wire}"
    );
    assert!(
        wire.contains("response_format: gfm_markdown")
            && wire.contains("response_structure: adaptive")
            && wire.contains("repository_evidence: source_first")
            && wire.contains("evidence_scope: source_files"),
        "the protected card must combine general Markdown/source-first policy with the \
         request's explicit code-file scope: {wire}"
    );
    // The evidence turn ANSWERED the line-count question through `find` — line
    // counts, descending — no shell, no `wc -l`, no bytesize substitute.
    assert!(
        wire.contains("120\\ttall.rs") || wire.contains("120\ttall.rs"),
        "the lined find result must reach the model, lines-first: {wire}"
    );
    // Docs/lockfiles must NOT appear as lined find evidence when code:true.
    assert!(
        !(wire.contains("AGENTS.md\t")
            || wire.contains("\tAGENTS.md")
            || wire.contains("Cargo.lock\t")
            || wire.contains("\tCargo.lock")),
        "code:true must exclude docs/lockfiles from the ranking evidence: {wire}"
    );
    let tall = wire.find("120").expect("most lines present");
    let mid = wire.find("40\\tmid.rs").or_else(|| wire.find("40\tmid.rs"));
    let fat = wire.find("2\\tfat.rs").or_else(|| wire.find("2\tfat.rs"));
    assert!(
        mid.is_some_and(|m| tall < m),
        "descending line-count order on the wire (tall before mid)"
    );
    assert!(
        fat.is_some_and(|f| mid.expect("mid present") < f),
        "descending line-count order: mid before fat (2 lines)"
    );
    // Anti-bytesize: a size-sort would put fat.rs FIRST with its byte count.
    // Seeing that size-first metric on the wire means we fell back to bytes —
    // the operator-classed failure mode.
    let fat_bytes = std::fs::metadata(ws.path().join("fat.rs"))
        .expect("fat.rs")
        .len();
    assert!(
        !(wire.contains(&format!("{fat_bytes}\\tfat.rs"))
            || wire.contains(&format!("{fat_bytes}\tfat.rs"))),
        "must NOT answer with fat.rs's byte size ({fat_bytes}) — that is the \
         bytesize-fallback failure: {wire}"
    );
    assert_clean_footer(hallucinations, end_reason);
}

/// The broad contract behind the incident regressions: ordinary repository
/// understanding starts from registered source files and returns structured
/// Markdown even when the prompt says nothing about lines, ranking, tables, or
/// a particular language.
#[tokio::test]
async fn general_repository_explanation_is_markdown_and_source_first() {
    let ws = simulated_workspace();
    let (reply, hallucinations, end_reason, wire) = run_scenario_for(
        GENERAL_REPOSITORY_PROMPT,
        ws.path(),
        vec![
            serde_json::json!({
                "content": null,
                "tool_calls": [{
                    "id": "c1", "type": "function",
                    "function": { "name": "find",
                        "arguments": "{\"path\":\".\",\"category\":\"source\",\"type\":\"f\",\"max_results\":20}" }
                }]
            }),
            serde_json::json!({ "content":
                "## Authentication implementation\n\n- Source evidence lives in `large.rs`.\n- No repository metadata was substituted for code." }),
        ],
    )
    .await;

    assert!(
        reply.starts_with("## Authentication implementation\n\n- "),
        "general repository findings must remain renderable GFM: {reply}"
    );
    assert!(
        wire.contains("response_format: gfm_markdown")
            && wire.contains("response_structure: adaptive")
            && wire.contains("repository_evidence: source_first")
            && wire.contains("source_definition: resolved_language_packs"),
        "standing response/repository policy must reach the model without incident keywords: \
         {wire}"
    );
    assert!(
        wire.contains("\\\"category\\\":\\\"source\\\"")
            || wire.contains("\"category\":\"source\""),
        "the general repository investigation must use the harness source category: {wire}"
    );
    assert_clean_footer(hallucinations, end_reason);
}

/// Flow 1c — the exact follow-up that regressed to an empty response. The
/// protected card resolves Rust through language-pack data, the tool call uses
/// the harness source category + language alias, and the final answer is a
/// syntactically complete GFM pipe table for the TUI renderer.
#[tokio::test]
async fn rust_followup_answers_with_source_filtered_markdown_table() {
    let ws = simulated_line_workspace();
    let (reply, hallucinations, end_reason, wire) = run_scenario_for(
        RUST_TABLE_PROMPT,
        ws.path(),
        vec![
            serde_json::json!({
                "content": null,
                "tool_calls": [{
                    "id": "c1", "type": "function",
                    "function": { "name": "find",
                        "arguments": "{\"path\":\".\",\"category\":\"source\",\"language\":\"rust\",\"type\":\"f\",\"sort\":\"lines\",\"show_lines\":true,\"max_results\":10}" }
                }]
            }),
            serde_json::json!({ "content":
                "| Rust file | Lines |\n|---|---:|\n| `tall.rs` | 120 |\n| `mid.rs` | 40 |\n| `fat.rs` | 2 |" }),
        ],
    )
    .await;

    assert!(
        reply.starts_with("| Rust file | Lines |\n|---|---:|"),
        "final answer must be a complete GFM table: {reply}"
    );
    assert!(
        wire.contains("source_extensions: rs")
            && wire.contains("source_filter: category=source language=rust"),
        "the protected card must resolve Rust through language-pack data: {wire}"
    );
    assert!(
        (wire.contains("\\\"category\\\":\\\"source\\\"")
            || wire.contains("\"category\":\"source\""))
            && (wire.contains("\\\"language\\\":\\\"rust\\\"")
                || wire.contains("\"language\":\"rust\"")),
        "the concrete find call must preserve source category + language: {wire}"
    );
    assert_clean_footer(hallucinations, end_reason);
}

/// Flow 2b — line-count boxed-in path: the model reaches for `wc -l` (the
/// shell answer to line counts). Research disposition-denies `run_command`
/// honestly; the formal escalation stays available. Same shape as Flow 2 for
/// the `du` pipeline — locks that line count does NOT require Act.
#[tokio::test]
async fn line_count_question_wc_denied_then_escalates_cleanly() {
    let ws = simulated_line_workspace();
    let (reply, hallucinations, end_reason, wire) = run_scenario_for(
        LINE_COUNT_PROMPT,
        ws.path(),
        vec![
            serde_json::json!({
                "content": null,
                "tool_calls": [{
                    "id": "c1", "type": "function",
                    "function": { "name": "run_command",
                        "arguments": "{\"command\":\"find . -name \\\"*.rs\\\" -type f -print0 | xargs -0 wc -l | sort -rn | head 10\"}" }
                }]
            }),
            serde_json::json!({
                "content": null,
                "tool_calls": [{
                    "id": "c2", "type": "function",
                    "function": { "name": "request_user_input",
                        "arguments": "{\"question\":\"May I run wc -l, or should I use find sort=lines?\"}" }
                }]
            }),
            serde_json::json!({ "content": "Proceeding with find sort=lines as suggested." }),
        ],
    )
    .await;

    assert!(
        reply.contains("Proceeding") || reply.contains("sort=lines"),
        "final answer returned: {reply}"
    );
    assert!(
        wire.contains("Tool `run_command` is unavailable"),
        "Research refuses the wc -l shell honestly: {wire}"
    );
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
