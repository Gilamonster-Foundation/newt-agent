use super::*;
use crate::caveats::Caveats;
use crate::{BackendKind, MemMessage};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Was the `"tools"` key present on this request body?
fn request_has_tools(req: &Request) -> bool {
    serde_json::from_slice::<serde_json::Value>(&req.body)
        .ok()
        .map(|v| v.get("tools").is_some())
        .unwrap_or(false)
}

/// Ollama-shaped responder: returns a tool call whenever `tools` are
/// offered, and a plain text answer once they are withheld. Counts the
/// number of tool-offering requests it served.
struct OllamaResponder {
    tool_rounds_served: Arc<AtomicUsize>,
    final_answer: String,
}

impl Respond for OllamaResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if request_has_tools(req) {
            self.tool_rounds_served.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "function": { "name": "definitely_not_a_real_tool", "arguments": {} }
                    }]
                }
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": self.final_answer }
            }))
        }
    }
}

fn msgs() -> Vec<MemMessage> {
    vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ]
}

fn hard_budget_ctx<'a>(
    url: &'a str,
    messages: &'a [MemMessage],
    caveats: &'a Caveats,
    task: &'a str,
    kind: BackendKind,
) -> ChatCtx<'a> {
    ChatCtx {
        url,
        model: "tiny-context-model",
        kind,
        api_key: (kind == BackendKind::Openai).then_some("sk-test"),
        messages,
        task,
        workspace: ".",
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
        max_tool_rounds: 1,
        narration_nudge_cap: 1,
        action_nudges: true,
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
        inference_timeout_secs: 5,
        mid_loop_trim_threshold: 40,
        compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
        mid_loop_trim_tokens: None,
        max_ok_input: None,
        build_check_cmd: None,
        safe_context: Some(256),
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

async fn assert_no_requests(server: &MockServer) {
    assert!(
        server
            .received_requests()
            .await
            .expect("wiremock request journal")
            .is_empty(),
        "irreducible-prompt refusal must happen before HTTP dispatch"
    );
}

fn giant_prompt_messages(task: &str) -> Vec<MemMessage> {
    vec![MemMessage::system("base policy"), MemMessage::user(task)]
}

/// Build a history (~36k estimated tokens) far larger than the recovered
/// cw-400 budget, so the recovery's compaction is forced to FIRE (not merely
/// fit) even after the always-advertised tool schemas (~5k tokens) claim their
/// share of the recovered 40k-token window.
fn overflowing_responses_history(task: &str) -> Vec<MemMessage> {
    let mut messages = vec![MemMessage::system("base policy")];
    for i in 0..60 {
        messages.push(MemMessage::user(format!(
            "historical step {i} {}",
            "x".repeat(1_200)
        )));
        messages.push(MemMessage::assistant(format!(
            "did step {i} {}",
            "y".repeat(1_200)
        )));
    }
    messages.push(MemMessage::user(task));
    messages
}

// Shared fixtures stay private here; behavior suites inherit them via super::*.

#[cfg(test)]
#[path = "tool_round_cap/cap_exit.rs"]
mod cap_exit;

#[cfg(test)]
#[path = "tool_round_cap/authentication.rs"]
mod authentication;

#[cfg(test)]
#[path = "tool_round_cap/loop_controls.rs"]
mod loop_controls;

#[cfg(test)]
#[path = "tool_round_cap/responses_protocol.rs"]
mod responses_protocol;

#[cfg(test)]
#[path = "tool_round_cap/input_budget.rs"]
mod input_budget;

#[cfg(test)]
#[path = "tool_round_cap/responses_recovery.rs"]
mod responses_recovery;

#[cfg(test)]
#[path = "tool_round_cap/proactive_compaction.rs"]
mod proactive_compaction;

#[cfg(test)]
#[path = "tool_round_cap/compaction_transaction.rs"]
mod compaction_transaction;

#[cfg(test)]
#[path = "tool_round_cap/responses_validation.rs"]
mod responses_validation;

#[cfg(test)]
#[path = "tool_round_cap/chat_recovery.rs"]
mod chat_recovery;

#[cfg(test)]
#[path = "tool_round_cap/tool_events.rs"]
mod tool_events;

#[cfg(test)]
#[path = "tool_round_cap/misc.rs"]
mod misc;
