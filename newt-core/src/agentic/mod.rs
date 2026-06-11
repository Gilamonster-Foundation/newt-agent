//! The agentic loop — call model → execute tool calls → feed results back →
//! repeat — extracted verbatim from `newt-tui` in Step 9.7 so the same
//! battle-tested loop can serve both the TUI and the ACP worker (Step 9.8)
//! without being written twice.
//!
//! `ChatCtx` stays a **concrete** type: there is exactly one loop
//! implementation and no `InferenceBackend` trait (YAGNI per the roadmap —
//! revisit only when a second concrete backend exists). The only seam is
//! [`McpTools`], which breaks the `newt-core` ← `newt-mcp-client` dependency
//! cycle; see `mcp.rs`.

mod display;
mod mcp;
mod note_sink;
mod tools;
mod trim;
mod warmup;

pub use display::{print_newt, NEWT_ORANGE_CT};
pub use mcp::{McpTools, NoMcp};
pub use note_sink::{save_note_tool_definition, NoteNudge, NoteSink};
pub use tools::{execute_tool, tool_definitions, venv_cmd_prefix};
pub use warmup::warmup_if_cold;

use crate::retry::{with_backoff_notify, RetryPolicy};
use crossterm::{
    execute,
    style::{Color as CtColor, Print, ResetColor, SetForegroundColor},
};
use display::{emit_overflow_notice, print_debug, print_retry_indicator};
use std::io::{self, Write as _};
use tools::{is_hallucination, merged_tool_definitions};
use trim::{
    estimate_tokens, estimate_value_tokens, merge_round_usage, mid_loop_trim, ollama_usage,
    openai_usage, trim_for_summary, trim_to_token_budget, PromptTracker,
};

/// Retry policy for TUI inference calls: more patient than the hosted-API
/// default because local DGX nodes can drop for 30–60 s under load.
/// Total resilience window: ~90 s (2+4+8+16+30+30 s between attempts).
/// All thresholds are overridable via the standard `NEWT_HTTP_*` env vars.
fn tui_retry_policy() -> RetryPolicy {
    RetryPolicy::for_local_inference()
}

/// Hook recovering a hard context-window 400:
/// `(error, model, today) → new input-token cap`. See [`ChatCtx::recover_cw_400`].
pub type RecoverCw400 = fn(&anyhow::Error, &str, &str) -> Option<u32>;

/// Everything one agentic turn needs, resolved once by the caller (the TUI
/// resolves config + capability cache + caveats per turn and threads them in
/// here, so the loop itself never re-reads config from disk).
pub struct ChatCtx<'a> {
    pub url: &'a str,
    pub model: &'a str,
    /// Wire protocol of the active backend (Ollama vs OpenAI-compatible).
    pub kind: crate::BackendKind,
    /// Bearer token for authenticated OpenAI-compatible endpoints.
    pub api_key: Option<&'a str>,
    /// Full message list already assembled by `MemoryManager::build_messages`.
    pub messages: &'a [crate::MemMessage],
    pub task: &'a str,
    pub workspace: &'a str,
    pub color: bool,
    pub caveats: &'a crate::caveats::Caveats,
    /// Maximum tool-call rounds before forcing a final tools-disabled
    /// completion (from `[tui].max_tool_rounds`, default 25).
    pub max_tool_rounds: usize,
    /// Max lines of tool output shown inline (from `[tui].tool_output_lines`,
    /// default 20). Resolved once per turn and threaded to `execute_tool` so
    /// the tool loop never re-reads config from disk.
    pub tool_output_lines: usize,
    /// Enable per-round diagnostic output. Set via `NEWT_DEBUG=1` or the
    /// `[tui] debug = true` config key.
    pub debug: bool,
    /// Ollama `options.num_ctx` — caps KV-cache allocation to prevent VRAM
    /// exhaustion on large models. `None` → model default (often 131k).
    pub num_ctx: Option<u32>,
    /// TCP connect timeout. Short (5 s default) so a down endpoint fails fast
    /// rather than blocking the full `inference_timeout_secs`.
    pub connect_timeout_secs: u64,
    /// Total inference timeout. Must be long enough for the model to generate
    /// a complete response (120 s default).
    pub inference_timeout_secs: u64,
    /// Message list size at which the agent trims the middle of the in-flight
    /// conversation to prevent context overflow mid-turn.
    pub mid_loop_trim_threshold: usize,
    /// Estimated-token threshold that also triggers a mid-loop trim, regardless
    /// of message count. Guards against a single huge tool result blowing past
    /// the context window in one round (from `[tui].mid_loop_trim_tokens`).
    /// `None` disables token-based trimming. See issue #223.
    pub mid_loop_trim_tokens: Option<usize>,
    /// Highest input-token count this model has accepted without a 400, from
    /// `model-capabilities.json`. Used as the pre-send budget gate: requests
    /// estimated to exceed it are trimmed *before* dispatch. `None` falls back
    /// to `safe_context`. See issue #223.
    pub max_ok_input: Option<u32>,
    /// Shell command run after every successful file write to give the model
    /// immediate ground-truth feedback (e.g. "cargo check -q --workspace").
    /// `None` disables auto-checking. Set per-workspace in `.newt/config.toml`.
    pub build_check_cmd: Option<String>,
    /// Empirically derived safe context size for this model (input tokens).
    /// Used to detect likely overflow when the model returns an empty response.
    /// Sourced from `model-capabilities.json` via `ensure_context_window`.
    /// `None` disables overflow detection.
    pub safe_context: Option<u32>,
    /// Hook invoked when a dispatch fails, to recover a hard context-window
    /// 400: `(error, model, today) → new input-token cap`. The TUI wires its
    /// `recover_context_window_400` (which parses the endpoint's real limit
    /// and persists it to `model-capabilities.json` — that cache stays
    /// TUI-side with the probe module). `None` disables recovery: the error
    /// propagates exactly as it did when no limit could be parsed. See #223.
    pub recover_cw_400: Option<RecoverCw400>,
    /// Model-writable note store behind the `save_note` tool (Step 19.3,
    /// #248). `None` ⇒ the tool is not advertised and the loop never writes
    /// memory (eval / headless callers unaffected). The TUI passes a sink
    /// over its session `MemoryManager`, so `save_note` and `/remember`
    /// share one store, one security scan, one char budget.
    pub note_sink: Option<&'a mut dyn NoteSink>,
    /// Turn-counted memory-nudge state ([`NoteNudge`]), owned by the caller
    /// across user turns and lent to the loop per call. Consulted only when
    /// `note_sink` is present; `None` disables the nudge.
    pub note_nudge: Option<&'a mut NoteNudge>,
}

/// Main agentic loop: call model → execute tool calls → feed results back → repeat.
/// Returns `(reply_text, was_streamed, token_usage, hallucination_count)`.
/// When `was_streamed` is true the text was already printed token-by-token.
///
/// Token-usage semantics (Step 18.1): `input_tokens` is the **largest single
/// prompt** the backend evaluated across the turn's rounds — the truthful
/// "how full did the context get" figure that feeds the capability ratchet —
/// NOT the per-round sum, which double-counts history (every round's prompt
/// re-includes all prior rounds; the B3 baseline measured 5.4× inflation).
/// `output_tokens` is the sum across rounds (each completion is new).
pub async fn chat_complete(
    ctx: ChatCtx<'_>,
    mcp: &mut dyn McpTools,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>, u32)> {
    // OpenAI-compatible endpoints speak a different wire format (request,
    // tool_calls, and usage shapes all differ), so they get their own loop.
    if ctx.kind == crate::BackendKind::Openai {
        return openai_chat_complete(ctx, mcp).await;
    }
    let ChatCtx {
        url,
        model,
        kind: _,
        api_key: _,
        messages: mem_messages,
        task: _task,
        workspace,
        color,
        caveats,
        max_tool_rounds,
        tool_output_lines,
        debug,
        num_ctx,
        connect_timeout_secs,
        inference_timeout_secs,
        mid_loop_trim_threshold,
        mid_loop_trim_tokens,
        max_ok_input,
        build_check_cmd,
        safe_context,
        recover_cw_400,
        mut note_sink,
        mut note_nudge,
    } = ctx;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs))
        .timeout(std::time::Duration::from_secs(inference_timeout_secs))
        .build()?;
    let chat_url = format!("{}/api/chat", url.trim_end_matches('/'));
    let retry = tui_retry_policy();
    // The save_note tool is advertised only when a sink exists (Step 19.3).
    let advertise_save_note = note_sink.is_some();

    // Convert MemMessage list to Ollama JSON format.
    // The memory manager already included the current task as the last user message.
    let mut messages: Vec<serde_json::Value> = mem_messages
        .iter()
        .map(|m| serde_json::json!({"role": m.role.as_str(), "content": m.content}))
        .collect();

    // In-band memory nudge (Step 19.3): after `[memory] note_nudge_interval`
    // user turns with zero organic save_note use, append a one-line reminder
    // to this turn's user message. Only when a sink exists — without one the
    // save_note tool isn't even advertised.
    if note_sink.is_some() {
        if let Some(line) = note_nudge.as_deref_mut().and_then(NoteNudge::begin_turn) {
            append_nudge_line(&mut messages, &line);
        }
    }

    let mut accumulated_usage: Option<crate::TokenUsage> = None;
    let mut hallucination_count: u32 = 0;
    let mut overflow_retries: u32 = 0;
    // Hard context-window 400s recovered (parse limit → trim → retry). See #223.
    let mut cw_retries: u32 = 0;
    // Pre-send token budget gate: trim before dispatch when the current context
    // size exceeds the model's empirically-confirmed max input (or the safe
    // context). Mutable because a recovered 400 tightens it mid-turn. See #223.
    let mut send_budget: Option<usize> = max_ok_input.or(safe_context).map(|c| c as usize);
    // Tool schemas ride along in every request body; count them once (18.1).
    // Stable for the whole turn: the builtin + MCP tool set doesn't change
    // mid-turn, so hoisting out of the round loop is safe.
    let tools = merged_tool_definitions(mcp, advertise_save_note);
    let tool_tokens = estimate_value_tokens(&tools);
    // Truthful context-size tracker: anchors on the backend's last-reported
    // prompt token count, chars/4 + schema estimate as fallback (Step 18.1).
    let mut prompt_tracker = PromptTracker::new();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    // Consecutive rounds where the model only called read-only tools (no writes).
    // When this hits READ_ONLY_NUDGE_AFTER, a brief injected message tells the
    // model to stop exploring and start writing.
    let mut read_only_rounds: usize = 0;

    // Agentic loop — up to `max_tool_rounds` tool-call rounds.
    'round_loop: for round in 0..max_tool_rounds {
        if round > 0 {
            // Brief separator between rounds so user can follow the flow.
            if color {
                execute!(
                    io::stdout(),
                    SetForegroundColor(CtColor::DarkGrey),
                    Print("…\n"),
                    ResetColor
                )
                .ok();
            }
        }

        // Read-only round nudge: if the model has spent several consecutive
        // rounds only reading (list_dir / read_file / web_fetch / search /
        // use_skill) without writing anything, inject a brief reminder to
        // stop exploring and call edit_file or write_file.  This breaks the
        // "endless exploration → empty response" failure mode seen with some
        // local models (e.g. nemotron3:33b).
        const READ_ONLY_NUDGE_AFTER: usize = 3;
        if read_only_rounds >= READ_ONLY_NUDGE_AFTER {
            let remaining = max_tool_rounds.saturating_sub(round + 1);
            messages.push(serde_json::json!({
                "role": "user",
                "content": format!(
                    "[{read_only_rounds} consecutive read-only rounds with no file writes. \
                     Stop exploring. Call edit_file or write_file now to make the change. \
                     You have {remaining} round(s) remaining — spend them writing, not reading.]"
                )
            }));
            read_only_rounds = 0;
        }

        // Mid-loop context trim: prevent VRAM exhaustion on long tool-call
        // sessions by dropping old middle messages when the list grows large
        // by message count OR by current token count (issue #223). The token
        // figure is prompt-tokens-preferred (Step 18.1).
        {
            let before = messages.len();
            let current = prompt_tracker.current(&messages, Some(&tools));
            let (trimmed, fired) = mid_loop_trim(
                &messages,
                mid_loop_trim_threshold,
                mid_loop_trim_tokens,
                current,
            );
            if fired {
                messages = trimmed;
                prompt_tracker.invalidate();
                if debug {
                    print_debug(
                        &format!(
                            "mid-loop trim: {before} → {} messages (count_threshold={}, ~{} tokens)",
                            messages.len(),
                            mid_loop_trim_threshold,
                            estimate_tokens(&messages),
                        ),
                        color,
                    );
                }
            }
        }

        // Pre-send token budget guard: when the current context size — the
        // backend's last-reported prompt tokens plus the estimate of what was
        // appended since (or the full request estimate including tool schemas
        // when no report exists) — exceeds the model's confirmed input
        // ceiling, trim BEFORE dispatch so a huge single round can't trigger
        // a non-retryable 400 (issue #223, accounting fixed in Step 18.1).
        if let Some(budget) = send_budget {
            let current = prompt_tracker.current(&messages, Some(&tools));
            if current > budget {
                // The schemas ride along in every request body, so the
                // message list gets what's left of the budget after them.
                let (trimmed, fired) =
                    trim_to_token_budget(&messages, budget.saturating_sub(tool_tokens), 2);
                if fired {
                    if debug {
                        print_debug(
                            &format!(
                                "pre-send trim: ~{current} tokens → fit budget {budget} \
                                 (incl. ~{tool_tokens} tool-schema tokens)",
                            ),
                            color,
                        );
                    }
                    messages = trimmed;
                    prompt_tracker.invalidate();
                }
            }
        }

        // Tool-call rounds: stream:false (fast, just JSON).
        // Final text round: stream:true so the user sees tokens arrive.
        // We don't know which round is last, so we probe with stream:false first
        // and switch to streaming only when the model returns no tool calls.
        let body_no_stream = if let Some(ctx_size) = num_ctx {
            serde_json::json!({
                "model": model,
                "messages": messages,
                "stream": false,
                "tools": tools.clone(),
                "options": { "num_ctx": ctx_size },
            })
        } else {
            serde_json::json!({
                "model": model,
                "messages": messages,
                "stream": false,
                "tools": tools.clone(),
            })
        };

        // Retry the send+status+parse as one unit — a connection drop at any
        // of these steps is transient and worth retrying with backoff.
        let dispatch = with_backoff_notify(
            &retry,
            || async {
                let resp = client
                    .post(&chat_url)
                    .json(&body_no_stream)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("Ollama {status}: {text}");
                }
                resp.json::<serde_json::Value>()
                    .await
                    .map_err(anyhow::Error::from)
            },
            |attempt, delay| print_retry_indicator(attempt, delay, color),
        )
        .await;
        let json: serde_json::Value = match dispatch {
            Ok(j) => j,
            Err(e) => {
                // Graceful context-window 400 recovery: parse the model's real
                // limit, tighten the budget, trim, and retry once (issue #223).
                if cw_retries < 2 {
                    if let Some(new_cap) = recover_cw_400.and_then(|f| f(&e, model, &today)) {
                        emit_overflow_notice(
                            color,
                            accumulated_usage.as_ref(),
                            Some(new_cap),
                            model,
                            cw_retries + 1,
                        );
                        send_budget = Some(new_cap as usize);
                        messages = trim_to_token_budget(
                            &messages,
                            (new_cap as usize).saturating_sub(tool_tokens),
                            2,
                        )
                        .0;
                        prompt_tracker.invalidate();
                        cw_retries += 1;
                        continue 'round_loop;
                    }
                }
                return Err(e);
            }
        };

        // Merge token usage from this non-streaming probe round (input = max
        // single prompt, output = sum — Step 18.1) and anchor the context-size
        // tracker on the backend-reported prompt size of this dispatch.
        let round_usage = ollama_usage(&json);
        if let Some(u) = round_usage {
            prompt_tracker.record(u.input_tokens, messages.len());
        }
        accumulated_usage = merge_round_usage(accumulated_usage, round_usage);

        let message = &json["message"];
        // Capture the probe content now — it may be our only copy of the
        // model's reply if the subsequent streaming re-issue returns empty.
        let probe_content = message["content"].as_str().unwrap_or("").to_string();

        let tool_calls = message["tool_calls"].as_array();
        let has_tools = tool_calls.map(|tc| !tc.is_empty()).unwrap_or(false);

        if debug {
            let content_excerpt = if probe_content.is_empty() {
                "(empty)".to_string()
            } else {
                let chars: String = probe_content.chars().take(80).collect();
                if probe_content.len() > 80 {
                    format!("{chars}…")
                } else {
                    chars
                }
            };
            let tc_count = tool_calls.map(|tc| tc.len()).unwrap_or(0);
            let usage_str = match round_usage {
                Some(u) => format!("{} in / {} out", u.input_tokens, u.output_tokens),
                None => "no usage".into(),
            };
            print_debug(
                &format!(
                    "round {round} probe: tool_calls={tc_count} usage=[{usage_str}] content={content_excerpt:?}"
                ),
                color,
            );
        }

        if !has_tools {
            // No tool calls — re-issue with stream:true so the user sees tokens.
            // `messages` already contains the task; just replay with streaming.
            //
            // IMPORTANT: the probe round already generated the model's answer in
            // `probe_content`. The streaming re-issue is a *second* inference call
            // from the same history; if it returns empty (non-determinism, context
            // pressure, or model quirk) we fall back to the probe content so the
            // user never sees a silent blank response.
            let body_stream = if let Some(ctx_size) = num_ctx {
                serde_json::json!({
                    "model": model,
                    "messages": &messages,
                    "stream": true,
                    "tools": tools.clone(),
                    "options": { "num_ctx": ctx_size },
                })
            } else {
                serde_json::json!({
                    "model": model,
                    "messages": &messages,
                    "stream": true,
                    "tools": tools.clone(),
                })
            };
            // Retry the connection; if we connect successfully but the stream
            // drops mid-token, that's a separate (harder) failure mode.
            let sresp = with_backoff_notify(
                &retry,
                || async {
                    client
                        .post(&chat_url)
                        .json(&body_stream)
                        .send()
                        .await
                        .map_err(|e| anyhow::anyhow!("stream request failed: {e}"))
                },
                |attempt, delay| print_retry_indicator(attempt, delay, color),
            )
            .await?;

            if !sresp.status().is_success() {
                if debug {
                    print_debug("stream request non-2xx — using probe content", color);
                }
                return Ok((probe_content, false, accumulated_usage, hallucination_count));
            }
            let (streamed, stream_usage) = stream_response(sresp, color).await?;

            if streamed.is_empty() {
                // The streaming re-issue produced no tokens. Fall back to the
                // probe content rather than returning silence.
                if debug {
                    print_debug(
                        &format!(
                            "stream returned empty — falling back to probe content ({} chars)",
                            probe_content.len()
                        ),
                        color,
                    );
                }
                if probe_content.is_empty() {
                    // Both probe and stream are empty — likely context overflow.
                    // `input_tokens` is the largest single prompt evaluated this
                    // turn (Step 18.1), so the 85%-of-safe-context check now
                    // compares one real prompt against the window instead of a
                    // multi-round sum that inflated past it after ~2 rounds.
                    let merged = merge_round_usage(accumulated_usage, stream_usage);
                    let overflow_likely = merged
                        .as_ref()
                        .zip(safe_context)
                        .map(|(u, safe)| u.input_tokens >= safe * 85 / 100)
                        .unwrap_or(false);
                    if overflow_likely && overflow_retries < 2 {
                        emit_overflow_notice(
                            color,
                            merged.as_ref(),
                            safe_context,
                            model,
                            overflow_retries + 1,
                        );
                        // Trim aggressively: keep system + first 2 + last N/4 messages.
                        messages = trim_for_summary(&messages, 2, max_tool_rounds / 4);
                        prompt_tracker.invalidate();
                        accumulated_usage = merged;
                        overflow_retries += 1;
                        continue 'round_loop;
                    }
                    let msg = "(model returned an empty response — try rephrasing, or check the model with `newt doctor`)";
                    return Ok((msg.to_string(), false, merged, hallucination_count));
                }
                // Use probe content; print it since it was never streamed.
                return Ok((
                    probe_content,
                    false,
                    merge_round_usage(accumulated_usage, stream_usage),
                    hallucination_count,
                ));
            }

            return Ok((
                streamed,
                true,
                merge_round_usage(accumulated_usage, stream_usage),
                hallucination_count,
            ));
        }

        // Has tool calls — add assistant turn and execute them.
        messages.push(message.clone());
        let mut round_wrote = false;
        for tc in tool_calls.unwrap() {
            let name = tc["function"]["name"].as_str().unwrap_or("unknown");
            let args = match &tc["function"]["arguments"] {
                serde_json::Value::String(s) => {
                    serde_json::from_str(s).unwrap_or(serde_json::Value::Null)
                }
                v => v.clone(),
            };
            if is_hallucination(name, &args) {
                hallucination_count += 1;
            }
            if !is_read_only_tool(name) {
                round_wrote = true;
            }
            // Organic save_note use resets the memory-nudge counter (the
            // read-only-rounds reset pattern) — active curators never see it.
            if name == "save_note" && note_sink.is_some() {
                if let Some(n) = note_nudge.as_deref_mut() {
                    n.note_saved();
                }
            }
            let result = execute_tool(
                name,
                &args,
                workspace,
                color,
                tool_output_lines,
                caveats,
                mcp,
                build_check_cmd.as_deref(),
                // Reborrow + re-coerce: shortens the trait-object lifetime to
                // this call (Option<&mut dyn _> is invariant, so the longer
                // ChatCtx lifetime can't unify directly).
                note_sink
                    .as_deref_mut()
                    .map(|s| &mut *s as &mut dyn NoteSink),
            )
            .await;
            messages.push(serde_json::json!({
                "role": "tool",
                "content": result
            }));
        }
        if round_wrote {
            read_only_rounds = 0;
        } else {
            read_only_rounds = read_only_rounds.saturating_add(1);
        }
    }

    // Reached the round cap. Trim the bloated message list so the final
    // summary request doesn't overflow the model's context window, then
    // make ONE tools-disabled completion so the user gets a real partial answer.
    let trimmed = trim_for_summary(&messages, 2, 6);
    let (text, streamed, usage) = final_summary_ollama(
        &client,
        &chat_url,
        model,
        trimmed,
        max_tool_rounds,
        accumulated_usage,
    )
    .await?;
    Ok((text, streamed, usage, hallucination_count))
}

/// Returns `true` when `name` is a tool that doesn't modify the workspace.
/// Used to count consecutive read-only rounds and inject a write-nudge.
/// `save_note` writes *memory*, not the workspace — a round that only saved
/// a note must not suppress the stop-exploring-start-writing nudge.
fn is_read_only_tool(name: &str) -> bool {
    matches!(
        name,
        "list_dir" | "read_file" | "search" | "web_fetch" | "use_skill" | "save_note"
    )
}

/// Append the memory-nudge line to the current user message — the last
/// message in the list per the memory-manager contract. Defensive fallback:
/// if the last message somehow isn't a user turn, push a standalone user
/// message instead (mirrors the read-only-rounds nudge injection).
fn append_nudge_line(messages: &mut Vec<serde_json::Value>, line: &str) {
    match messages.last_mut() {
        Some(last) if last["role"] == "user" => {
            let cur = last["content"].as_str().unwrap_or_default();
            last["content"] = serde_json::Value::String(format!("{cur}\n\n{line}"));
        }
        _ => messages.push(serde_json::json!({"role": "user", "content": line})),
    }
}

/// Build the nudge appended to the message list when the tool-round cap is hit.
fn cap_exit_nudge(max_tool_rounds: usize) -> String {
    format!(
        "You have reached the tool-call limit ({max_tool_rounds} rounds). \
         Do NOT call any more tools. Summarize what you found across the tool \
         calls above and give your best final answer now."
    )
}

/// Fallback message returned when even the final tools-disabled completion
/// fails. Includes accumulated token counts so the user knows what was consumed,
/// and gives actionable advice rather than just naming the limit.
fn cap_exit_fallback(max_tool_rounds: usize, accumulated: Option<crate::TokenUsage>) -> String {
    let tokens_hint = match accumulated {
        Some(u) => format!(
            " ({} in / {} out tokens consumed across {max_tool_rounds} rounds)",
            u.input_tokens, u.output_tokens,
        ),
        None => String::new(),
    };
    format!(
        "(reached the tool-call limit of {max_tool_rounds} rounds{tokens_hint}, \
         and the final summarization request also failed — \
         raise [tui].max_tool_rounds in your config, or ask a more focused question)"
    )
}

/// Final tools-disabled completion for the Ollama (`/api/chat`) path.
///
/// `messages` is the already-trimmed list (caller uses `trim_for_summary`).
/// `accumulated` carries usage from the preceding tool-call rounds so it
/// survives even when this summary request fails.
async fn final_summary_ollama(
    client: &reqwest::Client,
    chat_url: &str,
    model: &str,
    mut messages: Vec<serde_json::Value>,
    max_tool_rounds: usize,
    accumulated: Option<crate::TokenUsage>,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>)> {
    messages.push(serde_json::json!({
        "role": "user",
        "content": cap_exit_nudge(max_tool_rounds),
    }));
    // No `tools` key => the model cannot emit tool calls.
    let body = serde_json::json!({
        "model": model,
        "messages": &messages,
        "stream": false,
    });
    let retry = tui_retry_policy();
    let result = with_backoff_notify(
        &retry,
        || async {
            let resp = client
                .post(chat_url)
                .json(&body)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("Ollama {status}: {text}");
            }
            resp.json::<serde_json::Value>()
                .await
                .map_err(anyhow::Error::from)
        },
        |_, _| {}, // no color context here; tracing::warn covers it
    )
    .await;
    match result {
        Ok(json) => {
            let content = json["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let total = merge_round_usage(accumulated, ollama_usage(&json));
            if content.is_empty() {
                Ok((
                    cap_exit_fallback(max_tool_rounds, accumulated),
                    false,
                    accumulated,
                ))
            } else {
                Ok((content, false, total))
            }
        }
        // On any failure (including exhausted retries), still return the
        // accumulated usage so the caller can log the tokens consumed.
        Err(_) => Ok((
            cap_exit_fallback(max_tool_rounds, accumulated),
            false,
            accumulated,
        )),
    }
}

/// Final tools-disabled completion for the OpenAI (`/v1/chat/completions`) path.
///
/// `messages` is the already-trimmed list (caller uses `trim_for_summary`).
/// `accumulated` carries usage from the preceding tool-call rounds.
async fn final_summary_openai(
    client: &reqwest::Client,
    chat_url: &str,
    model: &str,
    api_key: Option<&str>,
    mut messages: Vec<serde_json::Value>,
    max_tool_rounds: usize,
    accumulated: Option<crate::TokenUsage>,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>)> {
    messages.push(serde_json::json!({
        "role": "user",
        "content": cap_exit_nudge(max_tool_rounds),
    }));
    // Omit `tools` / `tool_choice` => the model cannot emit tool calls.
    let body = serde_json::json!({
        "model": model,
        "messages": &messages,
        "stream": false,
    });
    let retry = tui_retry_policy();
    let result = with_backoff_notify(
        &retry,
        || async {
            let mut req = client.post(chat_url).json(&body);
            if let Some(key) = api_key {
                req = req.bearer_auth(key);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("inference endpoint {status}: {text}");
            }
            resp.json::<serde_json::Value>()
                .await
                .map_err(anyhow::Error::from)
        },
        |_, _| {},
    )
    .await;
    match result {
        Ok(json) => {
            let content = json["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let total = merge_round_usage(accumulated, openai_usage(&json["usage"]));
            if content.is_empty() {
                Ok((
                    cap_exit_fallback(max_tool_rounds, accumulated),
                    false,
                    accumulated,
                ))
            } else {
                Ok((content, false, total))
            }
        }
        Err(_) => Ok((
            cap_exit_fallback(max_tool_rounds, accumulated),
            false,
            accumulated,
        )),
    }
}

/// OpenAI-compatible variant of [`chat_complete`]: the same agentic tool-call
/// loop, but over `POST {endpoint}/v1/chat/completions` with bearer auth and
/// the OpenAI `tool_calls` / `tool_call_id` / `usage` shapes.
///
/// Non-streaming for now — the final answer is returned (and printed by the
/// caller) rather than streamed token-by-token. Token-by-token SSE streaming
/// is a follow-up; functionally the loop is complete, including tools.
pub async fn openai_chat_complete(
    ctx: ChatCtx<'_>,
    mcp: &mut dyn McpTools,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>, u32)> {
    let ChatCtx {
        url,
        model,
        kind: _,
        api_key,
        messages: mem_messages,
        task: _task,
        workspace,
        color,
        caveats,
        max_tool_rounds,
        tool_output_lines,
        debug,
        num_ctx,
        connect_timeout_secs,
        inference_timeout_secs,
        mid_loop_trim_threshold,
        mid_loop_trim_tokens,
        max_ok_input,
        build_check_cmd,
        safe_context,
        recover_cw_400,
        mut note_sink,
        mut note_nudge,
    } = ctx;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs))
        .timeout(std::time::Duration::from_secs(inference_timeout_secs))
        .build()?;
    let chat_url = format!("{}/v1/chat/completions", url.trim_end_matches('/'));
    let retry = tui_retry_policy();
    // The save_note tool is advertised only when a sink exists (Step 19.3).
    let advertise_save_note = note_sink.is_some();

    let mut messages: Vec<serde_json::Value> = mem_messages
        .iter()
        .map(|m| serde_json::json!({"role": m.role.as_str(), "content": m.content}))
        .collect();

    // In-band memory nudge (Step 19.3) — mirrors the Ollama path.
    if note_sink.is_some() {
        if let Some(line) = note_nudge.as_deref_mut().and_then(NoteNudge::begin_turn) {
            append_nudge_line(&mut messages, &line);
        }
    }

    let mut accumulated_usage: Option<crate::TokenUsage> = None;
    let mut hallucination_count: u32 = 0;
    // Hard context-window 400s recovered (parse limit → trim → retry). See #223.
    let mut cw_retries: u32 = 0;
    // Pre-send token budget gate; tightened mid-turn by a recovered 400 (#223).
    let mut send_budget: Option<usize> = max_ok_input.or(safe_context).map(|c| c as usize);
    // Tool schemas ride along in every request body; count them once (18.1).
    let tools = merged_tool_definitions(mcp, advertise_save_note);
    let tool_tokens = estimate_value_tokens(&tools);
    // Truthful context-size tracker (prompt-tokens-preferred, Step 18.1).
    let mut prompt_tracker = PromptTracker::new();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Agentic loop — up to `max_tool_rounds` tool-call rounds (matches the Ollama path).
    'round_loop: for round in 0..max_tool_rounds {
        if round > 0 && color {
            execute!(
                io::stdout(),
                SetForegroundColor(CtColor::DarkGrey),
                Print("…\n"),
                ResetColor
            )
            .ok();
        }

        // Mid-loop context trim (mirrors Ollama path): fire on message count OR
        // current token count (issue #223; prompt-tokens-preferred, Step 18.1).
        {
            let before = messages.len();
            let current = prompt_tracker.current(&messages, Some(&tools));
            let (trimmed, fired) = mid_loop_trim(
                &messages,
                mid_loop_trim_threshold,
                mid_loop_trim_tokens,
                current,
            );
            if fired {
                messages = trimmed;
                prompt_tracker.invalidate();
                if debug {
                    print_debug(
                        &format!(
                            "mid-loop trim: {before} → {} messages (count_threshold={}, ~{} tokens)",
                            messages.len(),
                            mid_loop_trim_threshold,
                            estimate_tokens(&messages),
                        ),
                        color,
                    );
                }
            }
        }

        // Pre-send token budget guard (mirrors Ollama path): when the current
        // context size — backend-reported prompt tokens preferred, request
        // estimate including tool schemas as fallback — exceeds the confirmed
        // input ceiling, trim before dispatch so a huge single round can't
        // trigger the non-retryable 400 that crashed issue #223.
        if let Some(budget) = send_budget {
            let current = prompt_tracker.current(&messages, Some(&tools));
            if current > budget {
                let (trimmed, fired) =
                    trim_to_token_budget(&messages, budget.saturating_sub(tool_tokens), 2);
                if fired {
                    if debug {
                        print_debug(
                            &format!(
                                "pre-send trim: ~{current} tokens → fit budget {budget} \
                                 (incl. ~{tool_tokens} tool-schema tokens)",
                            ),
                            color,
                        );
                    }
                    messages = trimmed;
                    prompt_tracker.invalidate();
                }
            }
        }

        // OpenAI-compatible endpoints don't use Ollama's `options.num_ctx` —
        // context limits are configured server-side (vLLM --max-model-len).
        let body = serde_json::json!({
            "model": model,
            "messages": messages,
            "tools": tools.clone(),
            "tool_choice": "auto",
            "stream": false,
        });
        let _ = num_ctx; // not applicable for OpenAI-compatible endpoints
        let dispatch = with_backoff_notify(
            &retry,
            || async {
                let mut req = client.post(&chat_url).json(&body);
                if let Some(key) = api_key {
                    req = req.bearer_auth(key);
                }
                let resp = req
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("inference endpoint {status}: {text}");
                }
                resp.json::<serde_json::Value>()
                    .await
                    .map_err(anyhow::Error::from)
            },
            |attempt, delay| print_retry_indicator(attempt, delay, color),
        )
        .await;
        let json: serde_json::Value = match dispatch {
            Ok(j) => j,
            Err(e) => {
                // Graceful context-window 400 recovery: parse the model's real
                // limit, tighten the budget, trim, and retry once (issue #223).
                if cw_retries < 2 {
                    if let Some(new_cap) = recover_cw_400.and_then(|f| f(&e, model, &today)) {
                        emit_overflow_notice(
                            color,
                            accumulated_usage.as_ref(),
                            Some(new_cap),
                            model,
                            cw_retries + 1,
                        );
                        send_budget = Some(new_cap as usize);
                        messages = trim_to_token_budget(
                            &messages,
                            (new_cap as usize).saturating_sub(tool_tokens),
                            2,
                        )
                        .0;
                        prompt_tracker.invalidate();
                        cw_retries += 1;
                        continue 'round_loop;
                    }
                }
                return Err(e);
            }
        };
        // Merge per-round token usage (input = max single prompt, output =
        // sum — Step 18.1) and anchor the context-size tracker.
        let round_usage = openai_usage(&json["usage"]);
        if let Some(u) = round_usage {
            prompt_tracker.record(u.input_tokens, messages.len());
        }
        accumulated_usage = merge_round_usage(accumulated_usage, round_usage);

        let message = &json["choices"][0]["message"];

        let tool_calls = message["tool_calls"].as_array();
        let has_tools = tool_calls.map(|tc| !tc.is_empty()).unwrap_or(false);

        if debug {
            let content = message["content"].as_str().unwrap_or("");
            let excerpt: String = content.chars().take(80).collect();
            let tc_count = tool_calls.map(|tc| tc.len()).unwrap_or(0);
            let usage_str = match round_usage {
                Some(u) => format!("{} in / {} out", u.input_tokens, u.output_tokens),
                None => "no usage".into(),
            };
            print_debug(
                &format!(
                    "round {round}: tool_calls={tc_count} usage=[{usage_str}] content={excerpt:?}"
                ),
                color,
            );
        }

        if !has_tools {
            let content = message["content"].as_str().unwrap_or("").to_string();
            if content.is_empty() && debug {
                print_debug(
                    "empty content with no tool calls — model produced nothing",
                    color,
                );
            }
            let out = if content.is_empty() {
                "(model returned an empty response — try rephrasing, or check the model with `newt doctor`)".to_string()
            } else {
                content
            };
            return Ok((out, false, accumulated_usage, hallucination_count));
        }

        // Record the assistant turn verbatim (it carries the tool_calls), then
        // run each call and feed the result back keyed by its tool_call_id.
        messages.push(message.clone());
        for tc in tool_calls.unwrap() {
            let id = tc["id"].as_str().unwrap_or("");
            let name = tc["function"]["name"].as_str().unwrap_or("unknown");
            let args = match &tc["function"]["arguments"] {
                serde_json::Value::String(s) => {
                    serde_json::from_str(s).unwrap_or(serde_json::Value::Null)
                }
                v => v.clone(),
            };
            if is_hallucination(name, &args) {
                hallucination_count += 1;
            }
            // Organic save_note use resets the memory-nudge counter (mirrors
            // the Ollama path).
            if name == "save_note" && note_sink.is_some() {
                if let Some(n) = note_nudge.as_deref_mut() {
                    n.note_saved();
                }
            }
            let result = execute_tool(
                name,
                &args,
                workspace,
                color,
                tool_output_lines,
                caveats,
                mcp,
                build_check_cmd.as_deref(),
                // Reborrow + re-coerce: shortens the trait-object lifetime to
                // this call (Option<&mut dyn _> is invariant, so the longer
                // ChatCtx lifetime can't unify directly).
                note_sink
                    .as_deref_mut()
                    .map(|s| &mut *s as &mut dyn NoteSink),
            )
            .await;
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": id,
                "content": result,
            }));
        }
    }

    // Reached the round cap. Trim the message list and make ONE final
    // tools-disabled completion (matches the Ollama path).
    let trimmed = trim_for_summary(&messages, 2, 6);
    let (text, streamed, usage) = final_summary_openai(
        &client,
        &chat_url,
        model,
        api_key,
        trimmed,
        max_tool_rounds,
        accumulated_usage,
    )
    .await?;
    Ok((text, streamed, usage, hallucination_count))
}

/// Stream an Ollama NDJSON response, printing tokens as they arrive.
/// Returns `(accumulated_text, token_usage)`.
/// Token usage is extracted from the final chunk (`done: true`).
async fn stream_response(
    resp: reqwest::Response,
    color: bool,
) -> anyhow::Result<(String, Option<crate::TokenUsage>)> {
    let mut full = String::new();
    let mut started = false;
    let mut usage: Option<crate::TokenUsage> = None;

    let mut resp = resp;
    while let Some(chunk) = resp.chunk().await? {
        let text = String::from_utf8_lossy(&chunk);
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let token = json["message"]["content"].as_str().unwrap_or("");
            if !token.is_empty() {
                if !started {
                    if color {
                        execute!(
                            io::stdout(),
                            SetForegroundColor(NEWT_ORANGE_CT),
                            Print("▸  "),
                            ResetColor,
                        )
                        .ok();
                    } else {
                        print!("▸  ");
                    }
                    started = true;
                }
                print!("{token}");
                io::stdout().flush().ok();
                full.push_str(token);
            }
            if json["done"].as_bool().unwrap_or(false) {
                // Extract token counts from the final Ollama chunk.
                let input = json["prompt_eval_count"].as_u64().map(|n| n as u32);
                let output = json["eval_count"].as_u64().map(|n| n as u32);
                usage = input.zip(output).map(|(i, o)| crate::TokenUsage {
                    input_tokens: i,
                    output_tokens: o,
                });
                break;
            }
        }
    }
    if started {
        println!();
    }
    Ok((full, usage))
}

#[cfg(test)]
mod cap_exit_unit_tests {
    use super::*;

    #[test]
    fn cap_exit_nudge_names_the_limit() {
        let nudge = cap_exit_nudge(5);
        assert!(nudge.contains("5 rounds"), "got: {nudge}");
        assert!(nudge.contains("Do NOT call any more tools"));
    }

    #[test]
    fn cap_exit_fallback_includes_usage_when_present() {
        let with = cap_exit_fallback(
            4,
            Some(crate::TokenUsage {
                input_tokens: 12,
                output_tokens: 34,
            }),
        );
        assert!(with.contains("12 in / 34 out tokens"), "got: {with}");
        assert!(with.contains("max_tool_rounds"));

        let without = cap_exit_fallback(4, None);
        assert!(!without.contains("tokens consumed"), "got: {without}");
        assert!(without.contains("tool-call limit of 4"));
    }

    #[test]
    fn read_only_tools_classified_correctly() {
        // save_note writes memory, not the workspace: a round that only
        // saved a note must still count toward the read-only write-nudge.
        for name in &[
            "list_dir",
            "read_file",
            "search",
            "web_fetch",
            "use_skill",
            "save_note",
        ] {
            assert!(is_read_only_tool(name), "{name} should be read-only");
        }
    }

    #[test]
    fn write_tools_not_read_only() {
        for name in &["edit_file", "write_file", "run_command"] {
            assert!(!is_read_only_tool(name), "{name} should NOT be read-only");
        }
    }
}

// ---------------------------------------------------------------------------
// Tool-call round cap + graceful cap-exit (issue: configurable max_tool_rounds)
// ---------------------------------------------------------------------------
//
// These tests exercise both agentic loops (`chat_complete` -> Ollama path and
// `openai_chat_complete`) against a wiremock backend. The mock returns tool
// calls while `tools` are present in the request and a real text answer once
// they are absent — letting us assert that:
//   (1) the loop honours the configured `max_tool_rounds` cap, and
//   (2) on hitting the cap newt issues ONE final tools-disabled completion and
//       returns its text (NOT the `(reached tool-call limit)` placeholder).
//
// (The companion test that recovers a hard context-window 400 via the
// `recover_cw_400` hook lives in newt-tui — it exercises the TUI-side probe
// cache persistence under a HOME env guard.)
#[cfg(test)]
mod tool_round_cap_tests {
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

    /// OpenAI-shaped responder: same logic, OpenAI `choices[0].message` shape.
    struct OpenAiResponder {
        tool_rounds_served: Arc<AtomicUsize>,
        final_answer: String,
    }

    impl Respond for OpenAiResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if request_has_tools(req) {
                self.tool_rounds_served.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "definitely_not_a_real_tool", "arguments": "{}" }
                        }]
                    }}]
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": self.final_answer } }]
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

    #[tokio::test]
    async fn ollama_loop_honors_configured_cap_and_returns_real_final_answer() {
        let server = MockServer::start().await;
        let served = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(OllamaResponder {
                tool_rounds_served: served.clone(),
                final_answer: "here is my partial summary".into(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let cap = 3;
        let (reply, streamed, _usage, _hallu) = chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                caveats: &caveats,
                max_tool_rounds: cap,
                tool_output_lines: 20,
                debug: false,
                num_ctx: None,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("chat_complete should succeed");

        // The cap was honoured: exactly `cap` tool-offering rounds were served.
        assert_eq!(served.load(Ordering::SeqCst), cap);
        // The cap-exit issued a final tools-disabled completion and returned
        // its text — NOT the dead placeholder.
        assert_eq!(reply, "here is my partial summary");
        assert_ne!(reply, "(reached tool-call limit)");
        assert!(!streamed);
    }

    #[tokio::test]
    async fn openai_loop_honors_configured_cap_and_returns_real_final_answer() {
        let server = MockServer::start().await;
        let served = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(OpenAiResponder {
                tool_rounds_served: served.clone(),
                final_answer: "openai partial answer".into(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let cap = 2;
        let (reply, streamed, _usage, _hallu) = openai_chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Openai,
                api_key: Some("sk-test"),
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                caveats: &caveats,
                max_tool_rounds: cap,
                tool_output_lines: 20,
                debug: false,
                num_ctx: None,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("openai_chat_complete should succeed");

        assert_eq!(served.load(Ordering::SeqCst), cap);
        assert_eq!(reply, "openai partial answer");
        assert_ne!(reply, "(reached tool-call limit)");
        assert!(!streamed);
    }

    #[tokio::test]
    async fn cap_exit_fallback_when_final_summary_errors() {
        // No mock for the tools-disabled request would still 404 via the
        // tool-offering mock only matching when... actually both match the same
        // path, so instead we mount a server that always 500s the *second*
        // shape. Simpler: a server that returns tool calls for tools-present
        // and a 500 for tools-absent, forcing the fallback branch.
        let server = MockServer::start().await;
        let served = Arc::new(AtomicUsize::new(0));
        struct ErrOnFinal {
            served: Arc<AtomicUsize>,
        }
        impl Respond for ErrOnFinal {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                if request_has_tools(req) {
                    self.served.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": { "content": "", "tool_calls": [{
                            "function": { "name": "definitely_not_a_real_tool", "arguments": {} }
                        }]}
                    }))
                } else {
                    ResponseTemplate::new(500).set_body_string("boom")
                }
            }
        }
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ErrOnFinal {
                served: served.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let (reply, _streamed, _usage, _hallu) = chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                caveats: &caveats,
                max_tool_rounds: 2,
                tool_output_lines: 20,
                debug: false,
                num_ctx: None,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("chat_complete should succeed even when final summary errors");

        // Fallback names the limit + the knob — strictly better than the bare
        // placeholder.
        assert!(reply.contains("tool-call limit"));
        assert!(reply.contains("max_tool_rounds"));
    }

    /// `run_command` called with a tool name as the first word must return a
    /// corrective error message, not shell it through agent-bridle.
    #[tokio::test]
    async fn run_command_refuses_tool_name_as_shell_command() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = Caveats::top();
        for tool in [
            "list_dir",
            "read_file",
            "write_file",
            "use_skill",
            "web_fetch",
        ] {
            let args = serde_json::json!({ "command": format!("{tool} some/path") });
            let out = execute_tool(
                "run_command",
                &args,
                &ws.path().to_string_lossy(),
                false,
                20,
                &caveats,
                &mut NoMcp,
                None,
                None,
            )
            .await;
            assert!(
                out.contains("is a tool, not a shell command"),
                "expected corrective message for '{tool}', got: {out}"
            );
        }
    }

    /// When the final summary 500s, the accumulated usage from the tool rounds
    /// must still be returned (not None), so usage.jsonl is not blank.
    #[tokio::test]
    async fn accumulated_usage_survives_summary_failure() {
        let server = MockServer::start().await;
        let served = Arc::new(AtomicUsize::new(0));

        struct UsageRoundsErrFinal {
            served: Arc<AtomicUsize>,
        }
        impl Respond for UsageRoundsErrFinal {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                if request_has_tools(req) {
                    self.served.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": { "content": "", "tool_calls": [{
                            "function": { "name": "definitely_not_a_real_tool", "arguments": {} }
                        }]},
                        // Ollama reports per-round usage even in non-streaming mode.
                        "prompt_eval_count": 100,
                        "eval_count": 20,
                    }))
                } else {
                    ResponseTemplate::new(500).set_body_string("boom")
                }
            }
        }

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(UsageRoundsErrFinal {
                served: served.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let cap = 2;
        let (reply, _streamed, usage, hallu) = chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                caveats: &caveats,
                max_tool_rounds: cap,
                tool_output_lines: 20,
                debug: false,
                num_ctx: None,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("chat_complete must succeed even when final summary errors");

        // The fallback reply must contain accumulated token counts.
        assert!(reply.contains("tool-call limit"), "got: {reply}");
        assert!(
            reply.contains("in / ") && reply.contains("out tokens"),
            "fallback must include accumulated token counts, got: {reply}"
        );

        // The usage returned must be non-None and reflect the rounds.
        let u = usage.expect("usage must be Some even when final summary fails");
        // SEMANTICS CHANGED in Step 18.1: each round's 100-token prompt
        // contained the same history, so the turn input is the largest single
        // prompt (100), not the 200 sum that double-counted it.
        assert_eq!(
            u.input_tokens, 100,
            "largest single prompt across 2 rounds, not the sum"
        );
        assert_eq!(
            u.output_tokens, 40,
            "2 rounds × 20 output tokens each = 40 total"
        );

        // Unknown tool calls during cap rounds counted as hallucinations.
        assert_eq!(
            hallu, cap as u32,
            "each round had one hallucinated tool call"
        );
    }

    // -----------------------------------------------------------------------
    // Read-only nudge injection test
    //
    // Scenario: model keeps calling list_dir (read-only) for 3 rounds.
    // On round 4 the harness injects the nudge.  The responder detects the
    // nudge text in the message list and returns a final text answer instead
    // of another tool call, proving the nudge reached the model.
    // -----------------------------------------------------------------------

    struct ReadOnlyNudgeResponder {
        /// Flipped to true the first time the responder sees the nudge text.
        nudge_seen: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Respond for ReadOnlyNudgeResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = serde_json::from_slice::<serde_json::Value>(&req.body).unwrap_or_default();
            let has_nudge = body["messages"]
                .as_array()
                .map(|msgs| {
                    msgs.iter().any(|m| {
                        m["content"]
                            .as_str()
                            .map(|c| c.contains("consecutive read-only rounds"))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false);

            if has_nudge {
                self.nudge_seen
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                // Return a plain text answer — no more tool calls.
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "nudge received, writing file now" }
                }))
            } else if request_has_tools(req) {
                // Keep returning list_dir calls until the nudge arrives.
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "tool_calls": [{ "function": {
                            "name": "list_dir",
                            "arguments": { "path": "." }
                        }}]
                    }
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "final summary" }
                }))
            }
        }
    }

    #[tokio::test]
    async fn read_only_nudge_injected_after_three_rounds() {
        let server = MockServer::start().await;
        let nudge_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ReadOnlyNudgeResponder {
                nudge_seen: nudge_seen.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let (reply, _streamed, _usage, _hallu) = chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "list all files",
                workspace: ".",
                color: false,
                caveats: &caveats,
                max_tool_rounds: 10,
                tool_output_lines: 5,
                debug: false,
                num_ctx: None,
                connect_timeout_secs: 5,
                inference_timeout_secs: 30,
                mid_loop_trim_threshold: 40,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("chat_complete should succeed");

        assert!(
            nudge_seen.load(std::sync::atomic::Ordering::SeqCst),
            "nudge was never injected after 3 consecutive read-only rounds"
        );
        assert_eq!(
            reply, "nudge received, writing file now",
            "model should have responded to the nudge with a final answer"
        );
    }
}

// ---------------------------------------------------------------------------
// HTTP-loop tests — streaming, overflow retry, mid-loop trim, and final
// summary, all against wiremock backends.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod http_loop_tests {
    use super::*;
    use crate::caveats::Caveats;
    use crate::{BackendKind, MemMessage};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn msgs() -> Vec<MemMessage> {
        vec![
            MemMessage::system("you are a test"),
            MemMessage::user("do the thing"),
        ]
    }

    fn ctx<'a>(
        server_uri: &'a str,
        messages: &'a [MemMessage],
        caveats: &'a Caveats,
    ) -> ChatCtx<'a> {
        ChatCtx {
            url: server_uri,
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages,
            task: "do the thing",
            workspace: ".",
            color: false,
            caveats,
            max_tool_rounds: 8,
            tool_output_lines: 20,
            debug: false,
            num_ctx: None,
            connect_timeout_secs: 5,
            inference_timeout_secs: 30,
            mid_loop_trim_threshold: 40,
            mid_loop_trim_tokens: None,
            max_ok_input: None,
            build_check_cmd: None,
            safe_context: None,
            recover_cw_400: None,
            note_sink: None,
            note_nudge: None,
        }
    }

    fn body_json(req: &Request) -> serde_json::Value {
        serde_json::from_slice(&req.body).unwrap_or_default()
    }

    fn is_stream(req: &Request) -> bool {
        body_json(req)["stream"].as_bool().unwrap_or(false)
    }

    fn ndjson(lines: &[serde_json::Value]) -> ResponseTemplate {
        let body: String = lines
            .iter()
            .map(|l| format!("{l}\n"))
            .collect::<Vec<_>>()
            .join("");
        ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/x-ndjson")
    }

    /// Probe (stream:false) answers with plain content; the streaming re-issue
    /// (stream:true) returns NDJSON tokens with usage on the `done` chunk.
    struct StreamHappyResponder;
    impl Respond for StreamHappyResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if is_stream(req) {
                ndjson(&[
                    serde_json::json!({"message": {"content": "Hello "}, "done": false}),
                    serde_json::json!({
                        "message": {"content": "world"}, "done": true,
                        "prompt_eval_count": 7, "eval_count": 3
                    }),
                ])
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "probe answer"},
                    "prompt_eval_count": 5, "eval_count": 2,
                }))
            }
        }
    }

    #[tokio::test]
    async fn ollama_streams_final_answer_and_merges_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(StreamHappyResponder)
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let (reply, streamed, usage, hallu) =
            chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
                .await
                .expect("chat_complete should succeed");

        assert_eq!(reply, "Hello world", "tokens accumulated across chunks");
        assert!(streamed, "the streaming path printed the tokens");
        let u = usage.expect("probe + stream usage merged");
        // SEMANTICS CHANGED in Step 18.1: both requests carried the same
        // conversation, so input is max(5, 7) = 7 — the old sum (12) counted
        // the shared history twice. Output is still 2 + 3 (new generation).
        assert_eq!(u.input_tokens, 7, "max(5 probe, 7 stream), not the sum");
        assert_eq!(u.output_tokens, 5, "2 (probe) + 3 (stream)");
        assert_eq!(hallu, 0);
    }

    /// The streaming re-issue produces no tokens — the loop must fall back to
    /// the probe round's content rather than returning silence.
    struct EmptyStreamResponder;
    impl Respond for EmptyStreamResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if is_stream(req) {
                ndjson(&[serde_json::json!({"message": {"content": ""}, "done": true})])
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "probe says hi"},
                    "prompt_eval_count": 5, "eval_count": 2,
                }))
            }
        }
    }

    #[tokio::test]
    async fn empty_stream_falls_back_to_probe_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(EmptyStreamResponder)
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let (reply, streamed, usage, _) =
            chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
                .await
                .expect("chat_complete should succeed");

        assert_eq!(reply, "probe says hi");
        assert!(!streamed, "fallback content was never streamed");
        assert_eq!(usage.unwrap().input_tokens, 5);
    }

    /// Probe AND stream both empty, with no safe-context hint → the loop gives
    /// the explicit empty-response diagnostic instead of silence.
    struct AllEmptyResponder;
    impl Respond for AllEmptyResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if is_stream(req) {
                ndjson(&[serde_json::json!({"message": {"content": ""}, "done": true})])
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"message": {"content": ""}}))
            }
        }
    }

    #[tokio::test]
    async fn fully_empty_response_yields_diagnostic_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(AllEmptyResponder)
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let (reply, streamed, _, _) =
            chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
                .await
                .expect("chat_complete should succeed");

        assert!(
            reply.contains("model returned an empty response"),
            "got: {reply}"
        );
        assert!(reply.contains("newt doctor"), "points at diagnostics");
        assert!(!streamed);
    }

    /// First round: empty content with token usage near the safe-context
    /// ceiling → the loop must emit the overflow notice, trim, and retry.
    /// Second round: a real answer.
    struct OverflowThenRecover {
        probes: Arc<AtomicUsize>,
    }
    impl Respond for OverflowThenRecover {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if is_stream(req) {
                // Streams mirror the probe sequence: empty first, content after.
                if self.probes.load(Ordering::SeqCst) <= 1 {
                    ndjson(&[serde_json::json!({
                        "message": {"content": ""}, "done": true,
                        "prompt_eval_count": 90, "eval_count": 1
                    })])
                } else {
                    ndjson(&[
                        serde_json::json!({"message": {"content": "recovered "}, "done": false}),
                        serde_json::json!({
                            "message": {"content": "after trim"}, "done": true,
                            "prompt_eval_count": 12, "eval_count": 4
                        }),
                    ])
                }
            } else {
                let n = self.probes.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": {"content": ""},
                        "prompt_eval_count": 90, "eval_count": 1,
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": {"content": "recovered after trim"},
                        "prompt_eval_count": 12, "eval_count": 4,
                    }))
                }
            }
        }
    }

    #[tokio::test]
    async fn context_overflow_trims_and_retries_then_recovers() {
        let server = MockServer::start().await;
        let probes = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(OverflowThenRecover {
                probes: probes.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        // Safe window of 100 input tokens: the empty round reported a 90-token
        // prompt, and 90 ≥ 85% of 100, so it is classified as likely overflow.
        // (Step 18.1: the check compares the largest single prompt against the
        // window — the old multi-round sum, 180 here, inflated past 85% after
        // two rounds on EVERY long turn, firing spurious overflow retries.)
        c.safe_context = Some(100);
        let (reply, streamed, usage, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert_eq!(
            probes.load(Ordering::SeqCst),
            2,
            "overflow must trigger exactly one trim-and-retry probe"
        );
        assert_eq!(reply, "recovered after trim");
        assert!(streamed);
        assert_eq!(
            usage
                .expect("accumulated usage survives the retry")
                .input_tokens,
            90,
            "largest single prompt across the overflowed + recovered rounds"
        );
    }

    /// Tool calls every round with a tiny trim threshold: the mid-loop trim
    /// must fire (observable as the omission placeholder reaching the model).
    struct TrimObservingResponder {
        trim_seen: Arc<AtomicBool>,
    }
    impl Respond for TrimObservingResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            let placeholder_present = body["messages"]
                .as_array()
                .map(|m| {
                    m.iter().any(|msg| {
                        msg["content"]
                            .as_str()
                            .map(|c| c.contains("earlier tool-call messages omitted"))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false);
            if placeholder_present {
                self.trim_seen.store(true, Ordering::SeqCst);
            }
            if body.get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "", "tool_calls": [{
                        "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                    }]}
                }))
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"message": {"content": "final after trim"}}))
            }
        }
    }

    #[tokio::test]
    async fn mid_loop_trim_fires_when_message_list_grows() {
        let server = MockServer::start().await;
        let trim_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(TrimObservingResponder {
                trim_seen: trim_seen.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        c.max_tool_rounds = 3;
        c.mid_loop_trim_threshold = 4;
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert!(
            trim_seen.load(Ordering::SeqCst),
            "the omission placeholder must have reached the model mid-loop"
        );
        assert_eq!(reply, "final after trim");
    }

    /// The cap-exit summary round returns 200 with EMPTY content: the loop
    /// must surface the named fallback, not the empty string.
    struct EmptyFinalSummary;
    impl Respond for EmptyFinalSummary {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if body_json(req).get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "", "tool_calls": [{
                        "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                    }]}
                }))
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"message": {"content": ""}}))
            }
        }
    }

    #[tokio::test]
    async fn empty_final_summary_yields_cap_fallback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(EmptyFinalSummary)
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        c.max_tool_rounds = 2;
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert!(reply.contains("tool-call limit of 2"), "got: {reply}");
        assert!(reply.contains("max_tool_rounds"), "names the knob");
    }

    // -----------------------------------------------------------------------
    // OpenAI-path coverage
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn chat_complete_dispatches_openai_kind_and_returns_first_round_answer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "openai says hi"}}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 4},
            })))
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        c.api_key = Some("sk-test");
        // Calling chat_complete (not openai_chat_complete) pins the dispatch.
        let (reply, streamed, usage, hallu) = chat_complete(c, &mut NoMcp)
            .await
            .expect("openai dispatch should succeed");

        assert_eq!(reply, "openai says hi");
        assert!(!streamed, "openai path is non-streaming");
        let u = usage.unwrap();
        assert_eq!((u.input_tokens, u.output_tokens), (10, 4));
        assert_eq!(hallu, 0);
    }

    #[tokio::test]
    async fn openai_empty_content_yields_diagnostic_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": ""}}]
            })))
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp).await.expect("should succeed");
        assert!(
            reply.contains("model returned an empty response"),
            "got: {reply}"
        );
    }

    /// OpenAI mirror of the Ollama cap-exit fallback: tool calls until the cap,
    /// then a 400 on the tools-disabled summary → the named fallback.
    struct OpenAiErrOnFinal;
    impl Respond for OpenAiErrOnFinal {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if body_json(req).get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "definitely_not_a_real_tool", "arguments": "{}"}
                        }]
                    }}]
                }))
            } else {
                ResponseTemplate::new(400).set_body_string("bad request")
            }
        }
    }

    #[tokio::test]
    async fn openai_cap_exit_fallback_when_final_summary_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(OpenAiErrOnFinal)
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        c.max_tool_rounds = 2;
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("must succeed even when the summary errors");
        assert!(reply.contains("tool-call limit of 2"), "got: {reply}");
        assert!(reply.contains("max_tool_rounds"));
    }
}

// ---------------------------------------------------------------------------
// save_note tool + memory nudge — loop integration (Step 19.3, #248)
// ---------------------------------------------------------------------------
//
// Wiremock-backed tests against both agentic loops, pinning:
//   (1) save_note is advertised iff a NoteSink is present, and a save_note
//       tool call routes through the sink with the result fed back;
//   (2) the in-band memory nudge is appended to the user message when due —
//       and ONLY when a sink exists;
//   (3) organic save_note use resets the nudge counter (the read-only-rounds
//       reset pattern, hermes's reset-on-memory-write).
#[cfg(test)]
mod save_note_loop_tests {
    use super::note_sink::tests::MockSink;
    use super::*;
    use crate::caveats::Caveats;
    use crate::{BackendKind, MemMessage};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn msgs() -> Vec<MemMessage> {
        vec![
            MemMessage::system("you are a test"),
            MemMessage::user("do the thing"),
        ]
    }

    fn ctx<'a>(
        server_uri: &'a str,
        messages: &'a [MemMessage],
        caveats: &'a Caveats,
    ) -> ChatCtx<'a> {
        ChatCtx {
            url: server_uri,
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages,
            task: "do the thing",
            workspace: ".",
            color: false,
            caveats,
            max_tool_rounds: 6,
            tool_output_lines: 20,
            debug: false,
            num_ctx: None,
            connect_timeout_secs: 5,
            inference_timeout_secs: 30,
            mid_loop_trim_threshold: 40,
            mid_loop_trim_tokens: None,
            max_ok_input: None,
            build_check_cmd: None,
            safe_context: None,
            recover_cw_400: None,
            note_sink: None,
            note_nudge: None,
        }
    }

    fn body_json(req: &Request) -> serde_json::Value {
        serde_json::from_slice(&req.body).unwrap_or_default()
    }

    fn advertised_tool_names(body: &serde_json::Value) -> Vec<String> {
        body["tools"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|d| d["function"]["name"].as_str())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
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

    /// Ollama-shaped responder: issues one save_note tool call, then a final
    /// text answer once the "note saved:" tool result is visible in history.
    /// Also records whether save_note was advertised and whether the memory
    /// nudge line reached the model.
    struct SaveNoteResponder {
        save_note_advertised: Arc<AtomicBool>,
        nudge_seen: Arc<AtomicBool>,
        final_answer: String,
    }

    impl Respond for SaveNoteResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if advertised_tool_names(&body).contains(&"save_note".to_string()) {
                self.save_note_advertised.store(true, Ordering::SeqCst);
            }
            if messages_contain(&body, "[system reminder:")
                && messages_contain(&body, "without a saved note")
            {
                self.nudge_seen.store(true, Ordering::SeqCst);
            }
            if messages_contain(&body, "note saved:") {
                // The tool result round-tripped — answer for real now.
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": self.final_answer }
                }))
            } else if body.get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "tool_calls": [{ "function": {
                            "name": "save_note",
                            "arguments": {
                                "action": "add",
                                "text": "user prefers vi keybindings"
                            }
                        }}]
                    }
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "final summary" }
                }))
            }
        }
    }

    #[tokio::test]
    async fn ollama_save_note_routes_to_sink_and_result_feeds_back() {
        let server = MockServer::start().await;
        let advertised = Arc::new(AtomicBool::new(false));
        let nudge_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(SaveNoteResponder {
                save_note_advertised: advertised.clone(),
                nudge_seen: nudge_seen.clone(),
                final_answer: "noted, moving on".into(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut sink = MockSink::default();
        let mut c = ctx(&uri, &messages, &caveats);
        c.note_sink = Some(&mut sink);
        let (reply, _streamed, _usage, hallu) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert!(
            advertised.load(Ordering::SeqCst),
            "save_note must be advertised when a sink is present"
        );
        assert_eq!(
            sink.calls,
            vec!["add:user prefers vi keybindings"],
            "the tool call must route through the sink"
        );
        assert_eq!(reply, "noted, moving on");
        assert_eq!(hallu, 0, "save_note is a real tool, not a hallucination");
        assert!(
            !nudge_seen.load(Ordering::SeqCst),
            "no nudge configured — none may be injected"
        );
    }

    /// Without a sink the tool must be absent from the advertised set, and a
    /// configured nudge must NOT be appended (absent-without-sink).
    struct NoSinkObserver {
        save_note_advertised: Arc<AtomicBool>,
        nudge_seen: Arc<AtomicBool>,
    }

    impl Respond for NoSinkObserver {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if advertised_tool_names(&body).contains(&"save_note".to_string()) {
                self.save_note_advertised.store(true, Ordering::SeqCst);
            }
            if messages_contain(&body, "[system reminder:") {
                self.nudge_seen.store(true, Ordering::SeqCst);
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": "plain answer" }
            }))
        }
    }

    #[tokio::test]
    async fn without_sink_no_tool_and_no_nudge_even_when_due() {
        let server = MockServer::start().await;
        let advertised = Arc::new(AtomicBool::new(false));
        let nudge_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(NoSinkObserver {
                save_note_advertised: advertised.clone(),
                nudge_seen: nudge_seen.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        // A nudge that is overdue (interval 1, one quiet turn already counted)…
        let mut nudge = NoteNudge::new(1);
        let _ = nudge.begin_turn();
        let mut c = ctx(&uri, &messages, &caveats);
        // …but NO sink: the loop must neither advertise nor nudge.
        c.note_nudge = Some(&mut nudge);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert_eq!(reply, "plain answer");
        assert!(
            !advertised.load(Ordering::SeqCst),
            "save_note advertised without a sink"
        );
        assert!(
            !nudge_seen.load(Ordering::SeqCst),
            "nudge injected without a sink"
        );
    }

    #[tokio::test]
    async fn nudge_appended_to_user_message_when_due() {
        let server = MockServer::start().await;
        let advertised = Arc::new(AtomicBool::new(false));
        let nudge_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(NoSinkObserver {
                save_note_advertised: advertised.clone(),
                nudge_seen: nudge_seen.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut sink = MockSink::default();
        // One quiet turn already elapsed → due on this (the next) turn.
        let mut nudge = NoteNudge::new(1);
        let _ = nudge.begin_turn();
        let mut c = ctx(&uri, &messages, &caveats);
        c.note_sink = Some(&mut sink);
        c.note_nudge = Some(&mut nudge);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert_eq!(reply, "plain answer");
        assert!(
            nudge_seen.load(Ordering::SeqCst),
            "the reminder line must reach the model on the due turn"
        );
    }

    #[tokio::test]
    async fn organic_save_resets_the_nudge_counter() {
        let server = MockServer::start().await;
        let advertised = Arc::new(AtomicBool::new(false));
        let nudge_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(SaveNoteResponder {
                save_note_advertised: advertised.clone(),
                nudge_seen: nudge_seen.clone(),
                final_answer: "done".into(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut sink = MockSink::default();
        let mut nudge = NoteNudge::new(1);
        let mut c = ctx(&uri, &messages, &caveats);
        c.note_sink = Some(&mut sink);
        c.note_nudge = Some(&mut nudge);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");
        assert_eq!(reply, "done");
        assert_eq!(sink.calls.len(), 1, "the model saved organically");

        // The turn included an organic save → the counter restarted, so the
        // next turn must NOT be nudged (without the save, interval=1 would
        // have made it due).
        assert!(
            nudge.begin_turn().is_none(),
            "organic save_note use must reset the nudge counter"
        );
    }

    /// OpenAI-shaped mirror: save_note advertised + routed, nudge appended.
    struct OpenAiSaveNoteResponder {
        save_note_advertised: Arc<AtomicBool>,
        nudge_seen: Arc<AtomicBool>,
    }

    impl Respond for OpenAiSaveNoteResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if advertised_tool_names(&body).contains(&"save_note".to_string()) {
                self.save_note_advertised.store(true, Ordering::SeqCst);
            }
            if messages_contain(&body, "[system reminder:") {
                self.nudge_seen.store(true, Ordering::SeqCst);
            }
            if messages_contain(&body, "note saved:") {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": "openai noted" } }]
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "save_note",
                                "arguments": "{\"action\":\"add\",\"text\":\"CI gate is just check\"}"
                            }
                        }]
                    }}]
                }))
            }
        }
    }

    #[tokio::test]
    async fn openai_save_note_routes_and_nudge_appends() {
        let server = MockServer::start().await;
        let advertised = Arc::new(AtomicBool::new(false));
        let nudge_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(OpenAiSaveNoteResponder {
                save_note_advertised: advertised.clone(),
                nudge_seen: nudge_seen.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut sink = MockSink::default();
        let mut nudge = NoteNudge::new(1);
        let _ = nudge.begin_turn(); // due on this turn
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        c.note_sink = Some(&mut sink);
        c.note_nudge = Some(&mut nudge);
        let (reply, _, _, hallu) = chat_complete(c, &mut NoMcp)
            .await
            .expect("openai loop should succeed");

        assert_eq!(reply, "openai noted");
        assert_eq!(sink.calls, vec!["add:CI gate is just check"]);
        assert!(advertised.load(Ordering::SeqCst));
        assert!(nudge_seen.load(Ordering::SeqCst));
        assert_eq!(hallu, 0);
    }

    /// A sink error (here: the 19.1 over-budget curator error) must round-trip
    /// to the model verbatim as the tool result so it can replace/remove and
    /// retry — pinned end-to-end through the loop.
    struct ErrorEchoResponder {
        error_seen_by_model: Arc<AtomicBool>,
    }

    impl Respond for ErrorEchoResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if messages_contain(&body, "Replace or remove existing entries first")
                && messages_contain(&body, "1. an existing entry")
            {
                self.error_seen_by_model.store(true, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "I will curate first" }
                }))
            } else if body.get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "tool_calls": [{ "function": {
                            "name": "save_note",
                            "arguments": { "action": "add", "text": "too big" }
                        }}]
                    }
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "final summary" }
                }))
            }
        }
    }

    #[tokio::test]
    async fn over_budget_error_round_trips_to_the_model() {
        let server = MockServer::start().await;
        let error_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ErrorEchoResponder {
                error_seen_by_model: error_seen.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut sink = MockSink {
            fail_with: Some(
                "NOTES.md is full: this write needs 99/50 chars. \
                 Replace or remove existing entries first.\nCurrent entries:\n  1. an existing entry"
                    .into(),
            ),
            ..Default::default()
        };
        let mut c = ctx(&uri, &messages, &caveats);
        c.note_sink = Some(&mut sink);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert_eq!(reply, "I will curate first");
        assert!(
            error_seen.load(Ordering::SeqCst),
            "the curator error (full entry list + instruction) must reach the model verbatim"
        );
    }
}
