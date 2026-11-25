//! Cap-exit orchestration — the context threaded into the final tools-disabled
//! summary when the tool-round limit is reached (Step 27.5). This module holds
//! the `CapExit` struct and all helper functions for building the nudge,
//! fallback, progress handoff, and final model reply.

use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

use crate::agentic::attempt_capture;
use crate::agentic::capability_check;
use crate::agentic::claim_check;
use crate::agentic::compress::CompressState;
use crate::agentic::context_recovery;
use crate::agentic::generation_bounds;
use crate::agentic::observability;
use crate::agentic::self_verify;
use crate::agentic::dispatch_with_decoder;
use crate::agentic::merge_round_usage;
use crate::agentic::preflight_full_message_request;
use crate::agentic::tui_retry_policy;
use crate::NudgeClassifier;
use crate::agentic::FinalReply;
use crate::agentic::context_recovery::OptionalRejection;
use crate::agentic::SolveObservation;
use crate::Scope;
use crate::TokenUsage;
use crate::ocap::DisclosureFilter;
use crate::agentic::self_verify::VerificationLedger;
use crate::agentic::claim_check::TurnClaims;
use crate::retry;
use crate::agentic::redact_model_facing;
use crate::agentic::observed_report;
use crate::agentic::request_report;
use crate::agentic::trim::{estimate_tokens, ollama_usage};

/// The cap-exit context threaded into a final tools-disabled summary (Step
/// 27.5): the round limit, accumulated usage, the count of failed tool calls
/// (drives honest advice), the salvaged `<plan>`/`<state>` progress, and the
/// #867 observed-paths manifest (verified paths from tool results, collected
/// before the trim could delete them).
pub struct CapExit {
    pub(crate) max_tool_rounds: usize,
    pub(crate) accumulated: Option<TokenUsage>,
    pub(crate) wasted_calls: usize,
    pub(crate) progress: Option<String>,
    pub(crate) observed: Vec<String>,
    /// Complete-request ceiling for the final tools-disabled dispatch.
    pub(crate) request_budget: Option<usize>,
    pub(crate) calibration: f32,
    pub(crate) estimation: crate::tokens::TokenEstimation,
    /// Ollama's request `options` (see [`ollama_options`]), repeated on every
    /// request including the tools-disabled cap exit. Ignored by OpenAI chat.
    pub(crate) ollama_options: Option<serde_json::Value>,
    /// Learning evidence from fresh counts of this immutable summary request.
    /// Admission still probes anew on every attempt; this is never a count cache.
    pub(crate) prompt_measurement: Mutex<Option<context_recovery::PromptMeasurement>>,
    /// Set when the harness wrote the cap-exit text itself (#2372).
    pub(crate) fell_back: AtomicBool,
}

impl CapExit {
    pub(crate) fn new(
        max_tool_rounds: usize,
        accumulated: Option<TokenUsage>,
        wasted_calls: usize,
        progress: Option<String>,
        observed: Vec<String>,
        request_budget: Option<usize>,
        calibration: f32,
        estimation: crate::tokens::TokenEstimation,
        ollama_options: Option<serde_json::Value>,
    ) -> Self {
        Self {
            max_tool_rounds,
            accumulated,
            wasted_calls,
            progress,
            observed,
            request_budget,
            calibration,
            estimation,
            ollama_options,
            prompt_measurement: Mutex::new(None),
            fell_back: AtomicBool::new(false),
        }
    }

    pub(crate) fn push_nudge(&self, messages: &mut Vec<serde_json::Value>) {
        messages.push(serde_json::json!({
            "role": "user",
            "content": cap_exit_nudge(self.max_tool_rounds, self.progress.as_deref(), &self.observed),
        }));
    }

    pub(crate) fn fits(&self, messages: &[serde_json::Value], model: &str) -> bool {
        preflight_full_message_request(
            messages,
            None,
            self.request_budget,
            self.calibration,
            self.estimation,
            model,
        )
        .is_ok()
    }

    pub(crate) fn fallback(&self) -> (FinalReply, bool, Option<TokenUsage>) {
        self.fell_back
            .store(true, std::sync::atomic::Ordering::Relaxed);
        (
            cap_exit_fallback(
                self.max_tool_rounds,
                self.accumulated,
                self.wasted_calls,
                self.progress.as_deref(),
            ),
            false,
            self.accumulated,
        )
    }

    pub(crate) fn record_measurement(
        &self,
        measurement: context_recovery::PromptMeasurement,
    ) -> anyhow::Result<()> {
        let mut observed = self
            .prompt_measurement
            .lock()
            .map_err(|_| anyhow::anyhow!("optional prompt measurement lock poisoned"))?;
        // The final request body is immutable across transport retries. Retain
        // its strongest count for learning, independent of each fresh admission.
        if observed
            .as_ref()
            .is_none_or(|previous| measurement.tokens > previous.tokens)
        {
            *observed = Some(measurement);
        }
        Ok(())
    }

    pub(crate) fn learn_measurement(&self, state: &mut CompressState) -> anyhow::Result<()> {
        if let Some(measurement) = *self
            .prompt_measurement
            .lock()
            .map_err(|_| anyhow::anyhow!("optional prompt measurement lock poisoned"))?
        {
            measurement.learn(state);
        }
        Ok(())
    }

    pub(crate) fn recover_rejection(
        &self,
        result: anyhow::Result<(FinalReply, bool, Option<TokenUsage>)>,
        observations: &mut Option<&mut SolveObservation>,
        state: &mut CompressState,
        round: usize,
        attempt: u32,
    ) -> anyhow::Result<(FinalReply, bool, Option<TokenUsage>)> {
        self.learn_measurement(state)?;
        if let Err(error) = &result {
            if let Some(rejection) = error.downcast_ref::<OptionalRejection>() {
                context_recovery::terminal_optional(
                    observations,
                    state,
                    self.calibration,
                    round,
                    attempt,
                    rejection.estimated_tokens,
                )?;
                observability::observe_harness_reply(observations);
                return Ok(self.fallback());
            }
        }
        if self.fell_back.load(std::sync::atomic::Ordering::Relaxed) {
            observability::observe_harness_reply(observations);
        }
        result
    }

    pub(crate) async fn finish<Fut>(
        &self,
        endpoint: &str,
        attempts: Option<attempt_capture::AttemptScope<'_>>,
        request: impl Fn() -> Fut,
        http_error_prefix: &str,
        estimated_tokens: usize,
        extract: impl FnOnce(serde_json::Value) -> (String, Option<TokenUsage>),
    ) -> anyhow::Result<(FinalReply, bool, Option<TokenUsage>)>
    where
        Fut: std::future::Future<Output = anyhow::Result<reqwest::RequestBuilder>>,
    {
        self.finish_with_decoder(
            endpoint,
            attempts,
            request,
            http_error_prefix,
            estimated_tokens,
            |bytes| Ok(serde_json::from_slice(bytes)?),
            extract,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn finish_with_decoder<Fut>(
        &self,
        endpoint: &str,
        attempts: Option<attempt_capture::AttemptScope<'_>>,
        request: impl Fn() -> Fut,
        http_error_prefix: &str,
        estimated_tokens: usize,
        decode: fn(&[u8]) -> anyhow::Result<serde_json::Value>,
        extract: impl FnOnce(serde_json::Value) -> (String, Option<TokenUsage>),
    ) -> anyhow::Result<(FinalReply, bool, Option<TokenUsage>)>
    where
        Fut: std::future::Future<Output = anyhow::Result<reqwest::RequestBuilder>>,
    {
        let retry = tui_retry_policy(endpoint);
        let result = dispatch_with_decoder(
            &retry,
            attempts,
            request,
            http_error_prefix,
            |_, _, _| {},
            None,
            decode,
        )
        .await;
        let result = match result {
            Err(error)
                if retry::classify(&error)
                    == retry::Retryability::ContextExceeded =>
            {
                return Err(error.context(OptionalRejection { estimated_tokens }));
            }
            other => other,
        };
        if let Err(error) = &result {
            if error.downcast_ref::<generation_bounds::Stopped>().is_some() {
                let (mut text, streamed, usage) = self.fallback();
                generation_bounds::append_notice(&mut text, error);
                return Ok((
                    text,
                    streamed,
                    merge_round_usage(usage, observability::reported_usage(error)),
                ));
            }
        }
        if let Ok((json, attempt)) = result {
            let limited = generation_bounds::output_limited(&json);
            let (content, usage) = extract(json);
            attempt_capture::complete(attempt.as_ref(), usage);
            let total = merge_round_usage(self.accumulated, usage);
            if limited {
                return Ok((
                    FinalReply::from(generation_bounds::output_limit_notice(content, usage)),
                    false,
                    total,
                ));
            }
            if self.fell_back.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok((FinalReply::from(content), false, total));
            }
            let progress = self.progress.clone();
            let reply = cap_exit_model_reply(self.max_tool_rounds, self.accumulated, &content, progress.as_deref());
            let total = merge_round_usage(self.accumulated, usage);
            Ok((reply, false, total))
        } else {
            match result {
                Ok((_json, _attempt)) => {
                    // This shouldn't happen - we handled Ok above
                    unreachable!()
                }
                Err(error) => self.recover_rejection(
                    Err(error),
                    &mut None,
                    &mut CompressState::default(),
                    0,
                    0,
                ),
            }
        }
    }
}

/// Build the nudge appended to the message list when the tool-round cap is hit.
/// `progress` (the `<plan>`/`<state>` working memory, Step 27.5) is folded in so
/// the model summarizes against what it actually accomplished; `observed`
/// (#867 Part A) is the verified-paths manifest collected across the rounds.
pub fn cap_exit_nudge(max_tool_rounds: usize, progress: Option<&str>, observed: &[String]) -> String {
    // #867: the message list was just trimmed (`trim_for_summary`), so most
    // of the evidence this summary should cite is GONE — the forensic session
    // showed a model reconstructing plausible-but-nonexistent file paths from
    // its priors at exactly this point. Constrain the summary to what is
    // still verbatim in context; absence must be stated, not papered over.
    let mut nudge = format!(
        "You have reached the tool-round limit ({max_tool_rounds} rounds). \
         Do NOT call any more tools. Give the operator a concise progress update \
         that separates completed and verified work, the current state or blocker, \
         and remaining work. The tool loop has stopped at the cap. Report whether \
         the operator's objective is complete or what remains; do not infer completion \
         merely because tools are disabled. Cite only file paths \
         that appear verbatim in the messages above — if the evidence you need \
         was in the omitted messages, say so plainly instead of reconstructing \
         file names or line numbers from memory. Do not narrate an intention to \
         keep working now (for example, \"let me read/edit/verify\"); list those \
         actions as remaining work and state that they have not run because the \
         round cap stopped further tool calls. Do not reproduce literal `<plan>` \
         or `<state>` tags from the working memory below. \
         Report an ACTION (an edit made, a test run or passing, a branch created, a \
         commit, a push, a PR opened) ONLY if a successful tool result above confirms \
         it — if you did not see the tool result, the action did not happen; list it \
         as remaining work instead. The workspace's real git state is checked against \
         this summary."
    );
    // #867 Part A: the ledger survived the trim — hand the model the REAL
    // manifest so grounded citation is possible, not just demanded.
    if !observed.is_empty() {
        nudge.push_str(
            "\n\nFile paths actually observed in tool results this run \
             (these exist — cite from this list):",
        );
        for p in observed {
            nudge.push_str("\n- ");
            nudge.push_str(p);
        }
    }
    if let Some(p) = progress {
        nudge.push_str(&format!("\n\nYour progress so far:\n{p}"));
    }
    nudge
}

/// Fallback message returned when even the final tools-disabled completion
/// fails. Includes accumulated token counts, salvages the `<plan>`/`<state>`
/// progress so partial work survives (Step 27.5), and gives HONEST advice: a run
/// dominated by failed tool calls is a tooling/permissions problem, not too few
/// rounds, so we don't blindly tell the user to raise the cap.
pub fn cap_exit_tokens_hint(max_tool_rounds: usize, accumulated: Option<TokenUsage>) -> String {
    match accumulated {
        Some(u) => format!(
            " ({max_tool_rounds} in / {} out tokens consumed across {max_tool_rounds} rounds)",
            u.output_tokens,
        ),
        None => String::new(),
    }
}

pub fn cap_exit_advice(max_tool_rounds: usize, wasted_calls: usize) -> &'static str {
    // If at least one failed tool call per round, the cap was thrash, not a
    // genuine need for more rounds.
    if wasted_calls >= max_tool_rounds.max(1) {
        "most of those rounds were spent on tool calls that failed — the model \
         could not find a working edit/shell path, which is usually a tooling or \
         permissions issue rather than too few rounds; check `newt doctor`"
    } else {
        "increase the tool-round limit for the next attempt, or ask a more focused question"
    }
}

pub fn cap_exit_progress_block(label: &str, progress: Option<&str>) -> String {
    match progress {
        Some(p) => {
            // The ledger is one step per line, but a bare newline inside a
            // Markdown paragraph is a soft break that the styled surface folds
            // into a space (`markdown/emitter.rs`, `Event::SoftBreak`). Two
            // trailing spaces are a CommonMark hard break: one step per
            // rendered line, and invisible on the plain surface, which prints
            // the bytes verbatim (`display::print_newt`).
            let body = format!("{label}:\n{}", humanize_cap_exit_progress(p));
            format!("\n\n{}", body.replace('\n', "  \n"))
        }
        None => String::new(),
    }
}

/// The plan and scratchpad are deliberately represented with lightweight XML
/// inside model context. Those tags are protocol framing, not user-facing UI;
/// deterministic cap fallbacks render the same saved content as ordinary text.
pub fn humanize_cap_exit_progress(progress: &str) -> String {
    progress
        .replace("<plan>", "Plan:\n")
        .replace("</plan>", "")
        .replace("<state>", "Current state:\n")
        .replace("</state>", "")
        .trim()
        .to_string()
}

pub fn cap_exit_fallback(
    max_tool_rounds: usize,
    accumulated: Option<TokenUsage>,
    wasted_calls: usize,
    progress: Option<&str>,
) -> FinalReply {
    let tokens_hint = cap_exit_tokens_hint(max_tool_rounds, accumulated);
    let advice = cap_exit_advice(max_tool_rounds, wasted_calls);
    let salvaged = cap_exit_progress_block("Progress captured before the summary failed", progress);
    FinalReply::harness(format!(
        "Paused at the tool-round limit ({max_tool_rounds} rounds){tokens_hint}. \
         The final summarization request also failed while preparing a progress \
         update; {advice}.{salvaged}"
    ))
}

/// Preserve the model-authored progress update while making the objective state
/// unambiguous. A pending-action classification is useful at a cap (it often
/// carries the best remaining-work handoff); the footer prevents those future
/// actions from being mistaken for completed work.
pub fn cap_exit_progress_handoff(
    max_tool_rounds: usize,
    accumulated: Option<TokenUsage>,
    content: &str,
    has_pending_actions: bool,
    progress: Option<&str>,
) -> FinalReply {
    let tokens_hint = cap_exit_tokens_hint(max_tool_rounds, accumulated);
    let pending = if has_pending_actions {
        " The proposed next actions in this update have not run yet."
    } else {
        ""
    };
    let captured = cap_exit_progress_block("Captured working state", progress);
    FinalReply::from(content.to_owned()).with_notice(format!(
        "The tool loop reached the tool-round limit ({max_tool_rounds} rounds){tokens_hint}. \
         This progress handoff preserves the model's update.{pending}{captured}"
    ))
}

pub fn cap_exit_summary_is_action_handoff(content: &str) -> bool {
    NudgeClassifier::load_default()
        .classify(content)
        .is_pending_action()
        || crate::agentic::looks_like_unverified_stale_file_blocker(content)
}

/// Leave a clean, completed model summary untouched. Only add the deterministic
/// handoff wrapper when the model still proposes work or core has structured
/// plan/state to preserve. Interactive callers use `TurnEndReason::RoundCap` to
/// add their own resume affordance independently of this textual shape.
pub fn cap_exit_model_reply(
    max_tool_rounds: usize,
    accumulated: Option<TokenUsage>,
    content: &str,
    progress: Option<&str>,
) -> FinalReply {
    let pending = cap_exit_summary_is_action_handoff(content);
    if pending || progress.is_some() {
        cap_exit_progress_handoff(max_tool_rounds, accumulated, content, pending, progress)
    } else {
        FinalReply::from(content.to_string())
    }
}

/// Apply the same evidence and disclosure gates to every provider's FINAL
/// answer for a turn — cap-exit handoff AND normal (no-tool) finish alike
/// (#1964: this used to run only on the cap-exit path, so a normal finish —
/// including a fabricated "verified working" report — returned unannotated).
/// The Responses wire used to bypass all three checks on cap-exit even
/// though the shared summary prompt promised the workspace would be
/// verified; the same promise holds for any final answer, not just one
/// forced out by the round cap.
pub fn finalize_final_text(
    text: impl Into<FinalReply>,
    workspace: &str,
    read_scope: &Scope<String>,
    capability_evidence: &capability_check::Evidence,
    disclosure: Option<&DisclosureFilter>,
    // #2683: HEAD captured before this turn's tool calls ran, so a
    // "committed" claim can be checked against whether HEAD actually moved.
    turn_claims: &TurnClaims<'_>,
    verification: &VerificationLedger,
) -> String {
    let mut reply = text.into();
    let source_draft = redact_model_facing(disclosure, reply.model.clone());
    reply.model = observed_report::assistant_prose(&source_draft);
    // Evaluate each gate against model prose only. Its appended diagnostics are
    // harness evidence, not words to attribute to the assistant.
    reply.annotation(capability_check::annotate_unobserved_probe(
        reply.model.clone(),
        capability_evidence,
    ));
    reply.annotation(turn_claims.annotate(
        reply.model.clone(),
        workspace,
        verification.claim_directories(),
        read_scope,
    ));
    reply.annotation(verification.annotate_cargo_claim(reply.model.clone()));
    reply.annotation(verification.annotate_pr_claim(reply.model.clone()));
    reply.annotation(verification.annotate_push_claim(reply.model.clone()));
    reply.model
}

/// Ollama's request `options` (#2312): the declared window as `num_ctx` and an
/// explicit output allowance as `num_predict`, its supported cap field. `None`
/// when neither applies, so such a body gains no `options` key at all.
pub fn ollama_options(num_ctx: Option<u32>, num_predict: Option<u32>) -> Option<serde_json::Value> {
    let mut options = serde_json::Map::new();
    if let Some(num_ctx) = num_ctx {
        options.insert("num_ctx".into(), num_ctx.into());
    }
    if let Some(num_predict) = num_predict {
        options.insert("num_predict".into(), num_predict.into());
    }
    (!options.is_empty()).then_some(serde_json::Value::Object(options))
}

/// Final summarization call for Ollama when the tool-round cap is hit.
pub(crate) async fn final_summary_ollama(
    client: &reqwest::Client,
    chat_url: &str,
    model: &str,
    mut messages: Vec<serde_json::Value>,
    cap: &CapExit,
    attempts: Option<attempt_capture::AttemptScope<'_>>,
    sink: Option<&request_report::ReportSink>,
) -> anyhow::Result<(FinalReply, bool, Option<crate::TokenUsage>)> {
    cap.push_nudge(&mut messages);
    if !cap.fits(&messages, model) {
        return Ok(cap.fallback());
    }
    // No `tools` key => the model cannot emit tool calls.
    let mut body = serde_json::json!({
        "model": model,
        "messages": &messages,
        "stream": false,
    });
    // #2937: capture summary send (no tools field — None distinguishes from []).
    if let Some(s) = sink {
        let sys_texts: Vec<&str> = messages
            .iter()
            .filter(|m| m["role"] == "system")
            .filter_map(|m| m["content"].as_str())
            .collect();
        s.observe_request(&sys_texts, None)?;
    }
    if let Some(options) = &cap.ollama_options {
        body["options"] = options.clone();
    }
    cap.finish(
        chat_url,
        attempts,
        || async { Ok(client.post(chat_url).json(&body)) },
        "Ollama",
        estimate_tokens(&messages, cap.estimation),
        |json| {
            // #385: strip inline reasoning; all-reasoning content takes the fallback.
            let (content, _reasoning) = crate::reasoning::split_reasoning(
                json["message"]["content"].as_str().unwrap_or(""),
            );
            (content, ollama_usage(&json))
        },
    )
    .await
}