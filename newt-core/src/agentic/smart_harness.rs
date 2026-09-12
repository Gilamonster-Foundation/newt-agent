//! The inference adapter for the reusable, content-addressed harness policy.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use agent_harness::{Session, Verdict};
use content_addressable::ContentId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::compress::SummarizeFn;

#[path = "smart_tool_completion.rs"]
mod tool_completion;
pub(crate) use tool_completion::{
    push_tool_resolution, push_tool_return, ToolBatch, ToolInvocation,
};

/// Operator-overridable auxiliary protocol and per-turn bounds.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdjudicationSettings {
    pub timeout_ms: u64,
    pub max_calls: usize,
    pub total_timeout_ms: u64,
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
    pub max_output_tokens: u32,
    pub initial_tool_bytes: usize,
    /// Optional shared system-role guidance for both auxiliary tasks.
    pub system_instruction: String,
    pub instruction: String,
    pub navigation_instruction: String,
    pub nudge: String,
}

impl AdjudicationSettings {
    /// The production evidence envelope, also used by fixture-based evaluations.
    pub fn classification_prompt(
        &self,
        reply: ContentId,
        text: &str,
        antecedent: Option<&str>,
        task: Option<&str>,
    ) -> String {
        let evidence = serde_json::json!({"reply_cid":reply.to_string(),"reply":text,"harness_antecedent":antecedent,"operator_task":task});
        format!("{}\n{}", self.instruction, evidence)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.timeout_ms > 0 && self.max_calls > 0,
            "adjudication bounds must be positive"
        );
        anyhow::ensure!(
            self.total_timeout_ms > 0
                && self.max_input_bytes > 0
                && self.max_output_bytes > 0
                && self.max_output_tokens > 0
                && self.initial_tool_bytes > 0,
            "auxiliary bounds must be positive"
        );
        anyhow::ensure!(
            !self.instruction.trim().is_empty()
                && !self.navigation_instruction.trim().is_empty()
                && !self.nudge.trim().is_empty(),
            "adjudication protocol must not be empty"
        );
        Ok(())
    }
}

impl Default for AdjudicationSettings {
    fn default() -> Self {
        #[derive(Deserialize)]
        struct Defaults {
            timeout_ms: u64,
            max_calls: usize,
            total_timeout_ms: u64,
            max_input_bytes: usize,
            max_output_bytes: usize,
            max_output_tokens: u32,
            initial_tool_bytes: usize,
            #[serde(default)]
            system_instruction: String,
            instruction: String,
            navigation_instruction: String,
            nudge: String,
        }
        let d: Defaults = toml::from_str(include_str!("smart_harness.toml"))
            .expect("bundled adjudication protocol is valid TOML");
        Self {
            timeout_ms: d.timeout_ms,
            max_calls: d.max_calls,
            total_timeout_ms: d.total_timeout_ms,
            max_input_bytes: d.max_input_bytes,
            max_output_bytes: d.max_output_bytes,
            max_output_tokens: d.max_output_tokens,
            initial_tool_bytes: d.initial_tool_bytes,
            system_instruction: d.system_instruction,
            instruction: d.instruction,
            navigation_instruction: d.navigation_instruction,
            nudge: d.nudge,
        }
    }
}

struct State {
    session: Session,
    request: Option<ContentId>,
    reply: Option<ContentId>,
    admission_rejection: Option<ContentId>,
    antecedent: Option<String>,
    operator_task: Option<String>,
    calls: usize,
    nudges: usize,
    verified: bool,
    auxiliary_elapsed: Duration,
    initial: Option<Vec<Value>>,
    deferred_failure: Option<String>,
}

/// Require the existing filesystem boundary before exposing a durable frame.
pub fn validate_isolation_runtime() -> anyhow::Result<()> {
    anyhow::ensure!(
        !super::tools::ocap_disabled() && !super::tools::full_access_requested(),
        "durable smart harness requires confined launch authority"
    );
    anyhow::ensure!(
        cfg!(target_os = "linux") && crate::ocap_l3_backend().1,
        "durable smart harness requires object-bound Linux filesystem tools and Landlock"
    );
    Ok(())
}

pub(super) struct FramePermissionGate<'a> {
    pub harness: &'a SmartHarness,
    pub workspace: &'a std::path::Path,
    pub inner: &'a mut dyn super::permissions::PermissionGate,
    pub refusal: Option<String>,
}

impl super::permissions::PermissionGate for FramePermissionGate<'_> {
    fn ask(
        &mut self,
        requests: &[super::permissions::PermissionRequest],
    ) -> super::permissions::PermissionDecision {
        use super::permissions::PermissionDecision;
        match self.inner.ask(requests) {
            PermissionDecision::Allow(caveats) => {
                match self
                    .harness
                    .validate_tool_authority(&caveats, self.workspace)
                {
                    Ok(()) => PermissionDecision::Allow(caveats),
                    Err(error) => {
                        self.refusal = Some(error.to_string());
                        PermissionDecision::Deny
                    }
                }
            }
            PermissionDecision::Deny => PermissionDecision::Deny,
        }
    }

    fn ask_question(&mut self, question: &str) -> super::permissions::HumanQuestionOutcome {
        self.inner.ask_question(question)
    }
}

/// Transport-free session plus a bounded, tool-less inference callback.
/// No mutex is held while the auxiliary model is running.
pub struct SmartHarness {
    state: Mutex<State>,
    complete: Arc<SummarizeFn>,
    spill: super::content_spill::SessionSpillStore,
    settings: AdjudicationSettings,
}

impl std::fmt::Debug for SmartHarness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmartHarness").finish_non_exhaustive()
    }
}

impl SmartHarness {
    pub fn new(
        session: Session,
        complete: Arc<SummarizeFn>,
        settings: AdjudicationSettings,
    ) -> anyhow::Result<Self> {
        settings.validate()?;
        let initial = session.restored_messages()?;
        let initial = (!initial.is_empty()).then_some(initial);
        Ok(Self {
            state: Mutex::new(State {
                session,
                request: None,
                reply: None,
                admission_rejection: None,
                antecedent: None,
                operator_task: None,
                calls: 0,
                nudges: 0,
                verified: false,
                auxiliary_elapsed: Duration::ZERO,
                initial,
                deferred_failure: None,
            }),
            complete,
            spill: super::content_spill::SessionSpillStore::new(*uuid::Uuid::new_v4().as_bytes()),
            settings,
        })
    }

    fn state(&self) -> anyhow::Result<MutexGuard<'_, State>> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("smart harness storage lock poisoned"))?;
        if let Some(error) = &state.deferred_failure {
            anyhow::bail!("{error}");
        }
        Ok(state)
    }

    pub(super) fn validate_tool_authority(
        &self,
        caveats: &crate::Caveats,
        workspace: &std::path::Path,
    ) -> anyhow::Result<()> {
        if let Some(checkpoint) = self.state()?.session.checkpoint_path() {
            validate_isolation_runtime()?;
            let directory = checkpoint
                .parent()
                .and_then(std::path::Path::parent)
                .ok_or_else(|| anyhow::anyhow!("frame checkpoint has no storage directory"))?;
            crate::config::SmartHarnessConfig::validate_frame_directory(
                directory, caveats, workspace,
            )?;
        }
        Ok(())
    }

    /// Start one primary turn without discarding its retained session graph.
    pub fn start_turn(&self) -> anyhow::Result<()> {
        let mut s = self.state()?;
        s.session
            .interrupt_tool_batch("previous turn ended before tool completion")?;
        s.session.start_turn();
        if s.initial.is_none() {
            let history = s.session.restored_messages()?;
            s.initial = (!history.is_empty()).then_some(history);
        }
        s.calls = 0;
        s.nudges = 0;
        s.verified = false;
        s.auxiliary_elapsed = Duration::ZERO;
        s.antecedent = None;
        s.request = None;
        s.reply = None;
        s.admission_rejection = None;
        Ok(())
    }

    pub fn head(&self) -> anyhow::Result<ContentId> {
        Ok(self.state()?.session.head())
    }

    pub fn settings(&self) -> &AdjudicationSettings {
        &self.settings
    }

    pub(crate) fn spill_store(&self) -> &dyn super::content_spill::SpillStore {
        &self.spill
    }

    fn tool_slice(&self, cid: ContentId) -> anyhow::Result<Value> {
        let mut s = self.state()?;
        let max_bytes = self
            .settings
            .initial_tool_bytes
            .min(s.session.config().max_slice_bytes);
        let slice = s.session.re_read(&cid.to_string(), 0, max_bytes)?;
        Ok(serde_json::json!({"source_cid":cid,"slice":slice}))
    }

    /// Restore rich tool exchanges once, before the provider's wire conversion.
    pub(crate) fn initial_messages(&self, current: Vec<Value>) -> anyhow::Result<Vec<Value>> {
        let mut s = self.state()?;
        s.operator_task = current
            .last()
            .filter(|m| m["role"] == "user")
            .and_then(|m| m["content"].as_str())
            .map(str::to_owned);
        let messages = match s.initial.take() {
            Some(history) => {
                let mut messages = current
                    .iter()
                    .take_while(|m| matches!(m["role"].as_str(), Some("system" | "developer")))
                    .cloned()
                    .collect::<Vec<_>>();
                messages.extend(
                    history
                        .into_iter()
                        .filter(|m| !matches!(m["role"].as_str(), Some("system" | "developer"))),
                );
                if let Some(task) = current.last().filter(|m| m["role"] == "user") {
                    messages.push(task.clone());
                }
                messages
            }
            None => current,
        };
        s.session.record_messages(&messages)?;
        Ok(messages)
    }

    #[cfg(test)]
    pub(super) fn replay_last_request(&self) -> anyhow::Result<Vec<u8>> {
        let s = self.state()?;
        Ok(s.session.replay(
            s.request
                .ok_or_else(|| anyhow::anyhow!("no request recorded"))?,
        )?)
    }

    /// Record the complete final request before a byte can be sent.
    pub(crate) fn request(&self, body: &Value, format: &str) -> anyhow::Result<Vec<u8>> {
        let mut s = self.state()?;
        let request = s.session.record_request(body.clone(), format)?;
        s.request = Some(request.id);
        s.reply = None;
        s.admission_rejection = None;
        Ok(request.bytes)
    }

    pub(crate) fn prepare_with_messages(
        &self,
        body: &Value,
        format: &str,
        messages: &[Value],
    ) -> anyhow::Result<Vec<u8>> {
        let mut s = self.state()?;
        let request = s
            .session
            .record_rendered_request(body.clone(), format, messages)?;
        s.request = Some(request.id);
        s.reply = None;
        s.admission_rejection = None;
        Ok(request.bytes)
    }

    /// Admit the exact counter response as harness metadata for the prepared
    /// generation request. Counting never creates a primary-model observation.
    pub(crate) fn record_token_count(
        &self,
        result: Result<&crate::backend_probe::TokenCount, &anyhow::Error>,
        budget: usize,
    ) -> anyhow::Result<()> {
        let diagnostic = match result {
            Ok(count) => format!(
                "{} tokens, budget {budget}, method {}",
                count.tokens, count.method
            ),
            Err(error) => format!("{error:#}"),
        };
        let committed = (|| -> anyhow::Result<()> {
            let mut s = self.state()?;
            // A failed new observation must not reuse an earlier admission.
            s.admission_rejection = None;
            let request = s
                .request
                .ok_or_else(|| anyhow::anyhow!("token count has no recorded request"))?;
            let (payload, rejected) = match result {
                Ok(count) => (
                    serde_json::json!({
                        "kind": "token_count",
                        "tokens": count.tokens,
                        "budget": budget,
                        "method": count.method,
                        "response_body": std::str::from_utf8(&count.response_bytes)?,
                    }),
                    count.tokens > budget,
                ),
                Err(error) => (
                    serde_json::json!({
                        "kind": "token_count_error",
                        "budget": budget,
                        "error": format!("{error:#}"),
                    }),
                    crate::retry::classify(error) == crate::retry::Retryability::ContextExceeded,
                ),
            };
            let event = s
                .session
                .record_request_intervention(request, &serde_json::to_vec(&payload)?)?;
            s.admission_rejection = rejected.then_some(event);
            Ok(())
        })();
        committed.map_err(|error| {
            let message =
                format!("token-count evidence could not be committed ({diagnostic}): {error}");
            error.context(message)
        })
    }

    /// Even malformed, refused, or interrupted responses remain observations.
    pub(crate) fn observe(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let mut s = self.state()?;
        Self::record_observation(&mut s, bytes)
    }

    fn record_observation(s: &mut State, bytes: &[u8]) -> anyhow::Result<()> {
        let request = s
            .request
            .ok_or_else(|| anyhow::anyhow!("reply has no recorded request"))?;
        s.reply = Some(s.session.record_reply(request, bytes)?);
        s.admission_rejection = None;
        Ok(())
    }

    /// A dropped transport future cannot return a persistence error directly.
    /// Retain it so the cancellation path fails closed instead of reporting a
    /// clean interruption without its already observed response bytes.
    fn observe_on_drop(&self, bytes: &[u8]) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.deferred_failure.is_some() {
            return;
        }
        if let Err(error) = Self::record_observation(&mut state, bytes) {
            state.deferred_failure = Some(format!(
                "interrupted response observation could not be recorded: {error}"
            ));
        }
    }

    pub(crate) async fn project(
        &self,
        messages: &[Value],
        max_bytes: usize,
    ) -> anyhow::Result<Vec<Value>> {
        if serde_json::to_vec(messages)?.len() <= max_bytes {
            return Ok(messages.to_vec());
        }
        let (request, prompt) = {
            let mut s = self.state()?;
            let catalog = s.session.catalog(messages, max_bytes)?;
            let prompt = format!("{}\n{}", self.settings.navigation_instruction, catalog);
            (
                s.session.record_navigation_request(&catalog, &prompt)?,
                prompt,
            )
        };
        let completion = {
            let mut elapsed = NavigationTimer {
                harness: self,
                started: Instant::now(),
                pending: Some(request),
            };
            let result = self.complete_bounded(prompt).await;
            elapsed.pending = None;
            result
        };
        let raw = match completion {
            Ok(raw) => raw,
            Err(error) => {
                self.state()?
                    .session
                    .record_navigation_failure(request, &error.to_string())?;
                return Err(error.context("AdjudicationFailure: navigation auxiliary failed"));
            }
        };
        let mut s = self.state()?;
        s.session.record_navigation_reply(request, &raw)?;
        let selection = self
            .admit_output(&raw)
            .and_then(|()| Ok(serde_json::from_str::<Vec<String>>(&raw)?))
            .and_then(|selected| {
                Ok(s.session
                    .project_selection(messages, &selected, max_bytes)?)
            });
        match selection {
            Ok(messages) => Ok(messages),
            Err(error) => {
                s.session
                    .record_navigation_failure(request, &error.to_string())?;
                Err(error.context("AdjudicationFailure: inadmissible navigation proposal"))
            }
        }
    }

    pub(crate) fn record_messages(&self, messages: &[Value]) -> anyhow::Result<()> {
        Ok(self.state()?.session.record_messages(messages)?)
    }

    pub(crate) fn tool_batch(
        &self,
        calls: &[super::tools::ValidatedCall],
        messages: &[Value],
    ) -> anyhow::Result<ToolBatch<'_>> {
        let mut s = self.state()?;
        let reply = s
            .reply
            .ok_or_else(|| anyhow::anyhow!("tool dispatch has no observation"))?;
        let calls = calls
            .iter()
            .map(|call| {
                serde_json::json!({
                    "id":call.call_id, "function":{"name":call.name,"arguments":call.args}
                })
            })
            .collect::<Vec<_>>();
        let ids = s.session.begin_tool_batch(reply, &calls, messages)?;
        Ok(ToolBatch::new(self, ids))
    }

    pub(crate) fn reject_tools(&self, reason: &str, recoverable: bool) -> anyhow::Result<()> {
        let mut s = self.state()?;
        let reply = s
            .reply
            .ok_or_else(|| anyhow::anyhow!("tool rejection has no observation"))?;
        let text = format!("tool-call batch rejected before execution: {reason}");
        s.session.record_failure(reply, &text)?;
        s.session.record_intervention(&text, reply)?;
        s.session.record_outcome(
            reply,
            if recoverable { "continue" } else { "failed" },
            &text,
        )?;
        Ok(())
    }

    pub(crate) fn record_responses_messages(
        &self,
        instructions: Option<&str>,
        input: &[Value],
    ) -> anyhow::Result<()> {
        let mut messages = Vec::with_capacity(input.len() + 1);
        if let Some(text) = instructions {
            messages.push(serde_json::json!({"role":"system","content":text}));
        }
        messages.extend_from_slice(input);
        self.record_messages(&messages)
    }

    pub(crate) fn outcome(&self, reason: crate::TurnEndReason, text: &str) -> anyhow::Result<()> {
        let control = match reason {
            crate::TurnEndReason::Completed => "deliver",
            crate::TurnEndReason::AwaitingOperator => "await_operator",
            crate::TurnEndReason::Cancelled => "cancelled",
            crate::TurnEndReason::Failed => "failed",
            _ => "incomplete",
        };
        let mut s = self.state()?;
        s.session.interrupt_tool_batch(control)?;
        if let Some(reply) = s.reply {
            s.session.record_outcome(reply, control, text)?;
            s.reply = None;
        }
        Ok(())
    }

    pub(crate) fn provider_failure(&self, error: &str) -> anyhow::Result<()> {
        let mut s = self.state()?;
        if let Some(reply) = s.reply {
            if s.session.pending_replies().contains(&reply) {
                s.session.record_failure(reply, error)?;
                s.session.record_outcome(reply, "failed", error)?;
                s.reply = None;
            }
        }
        Ok(())
    }

    /// Settle the observed rejected request and retain the harness's budget
    /// decision without adding synthetic tool output to the conversation.
    pub(crate) fn context_exceeded(
        &self,
        event: &super::observability::BehaviorSignal,
    ) -> anyhow::Result<()> {
        let mut s = self.state()?;
        let request = s
            .request
            .ok_or_else(|| anyhow::anyhow!("context overflow has no recorded request"))?;
        let rejection = s
            .reply
            .or(s.admission_rejection)
            .ok_or_else(|| anyhow::anyhow!("context overflow has no observed rejection"))?;
        let payload = serde_json::to_string(&serde_json::json!({
            "request_cid": request,
            "signal": event,
        }))?;
        // This outcome belongs to the rejected request, not the whole turn.
        // The next request may proceed only after all recovery evidence commits.
        if let Some(reply) = s.reply {
            s.session.record_failure(reply, "context_exceeded")?;
            s.session.record_outcome(reply, "failed", "")?;
        }
        s.session.record_intervention(&payload, rejection)?;
        s.reply = None;
        s.admission_rejection = None;
        Ok(())
    }

    async fn complete_bounded(&self, prompt: String) -> anyhow::Result<String> {
        let timeout = {
            let mut s = self.state()?;
            anyhow::ensure!(
                s.calls < self.settings.max_calls,
                "auxiliary call budget exhausted"
            );
            anyhow::ensure!(
                self.settings
                    .system_instruction
                    .len()
                    .checked_add(prompt.len())
                    .is_some_and(|bytes| bytes <= self.settings.max_input_bytes),
                "auxiliary input exceeds its byte budget"
            );
            let remaining = Duration::from_millis(self.settings.total_timeout_ms)
                .saturating_sub(s.auxiliary_elapsed);
            anyhow::ensure!(
                !remaining.is_zero(),
                "auxiliary elapsed-time budget exhausted"
            );
            s.calls += 1;
            remaining.min(Duration::from_millis(self.settings.timeout_ms))
        };
        let _elapsed = AuxiliaryTimer {
            harness: self,
            started: Instant::now(),
        };
        let raw = tokio::time::timeout(timeout, (self.complete)(prompt))
            .await
            .map_err(|_| anyhow::anyhow!("auxiliary timeout"))??;
        Ok(raw)
    }

    fn admit_output(&self, raw: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            raw.len() <= self.settings.max_output_bytes,
            "auxiliary output exceeds its byte budget"
        );
        Ok(())
    }

    pub(crate) fn read(&self, args: &Value) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Read {
            cid: String,
            #[serde(default)]
            offset: usize,
            #[serde(default = "default_read_bytes")]
            max_bytes: usize,
        }
        let args: Read = serde_json::from_value(args.clone())?;
        let value = self
            .state()?
            .session
            .re_read(&args.cid, args.offset, args.max_bytes)?;
        Ok(serde_json::to_string(&value)?)
    }

    fn failure(&self, reply: ContentId, reason: &str) -> anyhow::Result<Control> {
        self.state()?.session.record_failure(reply, reason)?;
        Ok(Control::Finish {
            text: format!("AdjudicationFailure: {reason}. The observed reply is retained; this turn is incomplete."),
            reason: crate::TurnEndReason::Failed,
        })
    }

    /// Classify an already-recorded observation; ancestry is evidence, never a verdict.
    pub(crate) async fn classify(
        &self,
        text: &str,
        nudge_cap: usize,
        more_rounds: bool,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> anyhow::Result<Control> {
        let (reply, request, prompt) = {
            let mut s = self.state()?;
            let reply = s
                .reply
                .ok_or_else(|| anyhow::anyhow!("cannot adjudicate an unrecorded reply"))?;
            s.session.record_model_message(reply, text)?;
            let prompt = self.settings.classification_prompt(
                reply,
                text,
                s.antecedent.as_deref(),
                s.operator_task.as_deref(),
            );
            (
                reply,
                s.session.record_adjudication_request(reply, &prompt)?,
                prompt,
            )
        };
        let result = super::cancellable(cancel, self.complete_bounded(prompt)).await;
        let raw = match result {
            None => {
                self.state()?
                    .session
                    .record_adjudication_failure(request, "cancelled")?;
                self.state()?
                    .session
                    .record_failure(reply, "adjudication cancelled")?;
                return Ok(Control::Finish {
                    text: text.to_string(),
                    reason: crate::TurnEndReason::Cancelled,
                });
            }
            Some(Err(error)) => {
                self.state()?
                    .session
                    .record_adjudication_failure(request, &error.to_string())?;
                return self.failure(reply, &format!("auxiliary unavailable: {error}"));
            }
            Some(Ok(raw)) => raw,
        };
        self.state()?
            .session
            .record_adjudication_reply(request, &raw)?;
        if let Err(error) = self.admit_output(&raw) {
            self.state()?
                .session
                .record_adjudication_failure(request, &error.to_string())?;
            return self.failure(reply, &error.to_string());
        }
        let verdict = match parse_verdict(&raw) {
            Some("answer") => Verdict::Answer,
            Some("narration") => Verdict::Narration,
            Some("question") => Verdict::Question,
            _ => return self.failure(reply, "malformed auxiliary verdict"),
        };
        let mut s = self.state()?;
        s.session.record_verdict(reply, verdict)?;
        match verdict {
            Verdict::Answer => Ok(Control::Answer),
            Verdict::Question => Ok(Control::Finish {
                text: text.to_string(),
                reason: crate::TurnEndReason::AwaitingOperator,
            }),
            Verdict::Narration if more_rounds && s.nudges < nudge_cap => {
                let nudge = format!(
                    "{} {}",
                    super::compress::LOOP_GUIDANCE_PREFIX,
                    self.settings.nudge
                );
                s.session.record_host_message(&nudge, reply)?;
                s.session.record_outcome(reply, "continue", &nudge)?;
                s.antecedent = Some(nudge.clone());
                s.nudges += 1;
                Ok(Control::Continue(nudge))
            }
            Verdict::Narration => Ok(Control::Finish {
                text: format!(
                    "{text}\n\nIncomplete: the model supplied narration without a deliverable."
                ),
                reason: if more_rounds {
                    crate::TurnEndReason::NarrationCapExhausted
                } else {
                    crate::TurnEndReason::NarrationFinalRound
                },
            }),
        }
    }

    pub(crate) fn host_message(&self, text: &str) -> anyhow::Result<()> {
        let mut s = self.state()?;
        let parent = s
            .reply
            .or_else(|| s.session.last_message())
            .ok_or_else(|| anyhow::anyhow!("host message has no recorded antecedent"))?;
        s.session.record_host_message(text, parent)?;
        s.antecedent = Some(text.to_string());
        Ok(())
    }

    fn host_envelope(&self, message: &Value) -> anyhow::Result<()> {
        let mut s = self.state()?;
        let parent = s
            .reply
            .or_else(|| s.session.last_message())
            .ok_or_else(|| anyhow::anyhow!("host tool result has no recorded antecedent"))?;
        s.session.record_host_envelope(message, parent)?;
        Ok(())
    }

    pub(crate) fn intervention(&self, text: &str) -> anyhow::Result<()> {
        let mut s = self.state()?;
        let parent = s
            .reply
            .ok_or_else(|| anyhow::anyhow!("intervention has no reply parent"))?;
        s.session.record_host_message(text, parent)?;
        s.session.record_outcome(parent, "continue", text)?;
        s.antecedent = Some(text.to_string());
        Ok(())
    }

    /// The auxiliary classification never replaces workspace verification.
    pub(crate) fn verify_answer(
        &self,
        control: Control,
        messages: &[Value],
        workspace: &str,
        task: &str,
        can_verify: bool,
    ) -> anyhow::Result<Control> {
        if matches!(control, Control::Answer)
            && can_verify
            && super::self_verify::enabled()
            && !self.state()?.verified
        {
            let entries = super::self_verify::workspace_entries(std::path::Path::new(workspace));
            let checks = super::self_verify::detect_checks(&entries, task);
            let commands = super::self_verify::commands_from_messages(messages);
            if let Some(text) = super::self_verify::verify_gate_nudge(&checks, &commands) {
                let text = format!("{} {text}", super::compress::LOOP_GUIDANCE_PREFIX);
                self.intervention(&text)?;
                self.state()?.verified = true;
                return Ok(Control::Continue(text));
            }
        }
        Ok(control)
    }
}

struct AuxiliaryTimer<'a> {
    harness: &'a SmartHarness,
    started: Instant,
}

struct NavigationTimer<'a> {
    harness: &'a SmartHarness,
    started: Instant,
    pending: Option<ContentId>,
}
impl Drop for NavigationTimer<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.harness.state.lock() {
            state
                .session
                .account_navigation_elapsed(self.started.elapsed());
            if let Some(request) = self.pending {
                if let Err(error) = state.session.record_navigation_failure(
                    request,
                    "navigation cancelled before auxiliary completion",
                ) {
                    state.deferred_failure = Some(format!(
                        "navigation cancellation could not be recorded: {error}"
                    ));
                }
            }
        }
    }
}
impl Drop for AuxiliaryTimer<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.harness.state() {
            state.auxiliary_elapsed += self.started.elapsed();
        }
    }
}

fn default_read_bytes() -> usize {
    4096
}

pub(crate) enum Control {
    Answer,
    Continue(String),
    Finish {
        text: String,
        reason: crate::TurnEndReason,
    },
}

pub(super) fn cancelled(
    harness: Option<&SmartHarness>,
    reason: &mut Option<&mut Option<crate::TurnEndReason>>,
) -> anyhow::Result<String> {
    if let Some(harness) = harness {
        harness.outcome(crate::TurnEndReason::Cancelled, "")?;
        if let Some(slot) = reason {
            **slot = Some(crate::TurnEndReason::Cancelled);
        }
    }
    Ok(String::new())
}

pub(crate) fn advertise(mut tools: Value, harness: Option<&SmartHarness>) -> Value {
    if harness.is_some() {
        if let Some(tools) = tools.as_array_mut() {
            tools
                .retain(|tool| !matches!(tool["function"]["name"].as_str(), Some("crew" | "find")));
            for tool in tools.iter_mut() {
                if tool["function"]["name"] == "run_command" {
                    tool["function"]["description"] = Value::String(
                        "Run a command in the confined workspace shell. File access must remain \
                         within fs_read/fs_write grants. Prefer dedicated file and lifecycle tools. \
                         Use shell find for recursive searches. \
                         Scoped shell Git reads and staging are supported; shell Git commits are \
                         refused to preserve harness attribution. Other commands with advertised \
                         dedicated tools must invoke those tools directly."
                            .into(),
                    );
                }
            }
            tools.push(
                serde_json::from_str(include_str!("re_read.json"))
                    .expect("bundled re_read tool is valid JSON"),
            );
        }
    }
    tools
}

/// Use exactly the recorded bytes on the transport; serde is never invoked twice.
pub(crate) fn request(
    builder: reqwest::RequestBuilder,
    body: &Value,
    harness: Option<&SmartHarness>,
    format: &str,
) -> anyhow::Result<reqwest::RequestBuilder> {
    match harness {
        Some(h) => Ok(builder
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(h.request(body, format)?)),
        None => Ok(builder.json(body)),
    }
}

pub(crate) async fn response(
    response: reqwest::Response,
    harness: Option<&SmartHarness>,
    prefix: &str,
) -> anyhow::Result<Value> {
    response_with_decoder(response, harness, prefix, |bytes| {
        Ok(serde_json::from_slice(bytes)?)
    })
    .await
}

/// OpenAI interpretation errors are model/wire evidence, with their original
/// error chain retained. JSON-only consumers retain their existing decoder.
pub(crate) fn decode_openai_response(bytes: &[u8]) -> anyhow::Result<Value> {
    super::openai_sse::decode_response(bytes).map_err(|error| {
        let classified = super::observability::DispatchError::http_status(format!("{error:#}"));
        error.context(classified)
    })
}

/// Own response bytes across suspension points. Dropping the reader is the
/// cancellation boundary, so its `Drop` records bytes already received before
/// the outer cancellation path commits the terminal outcome.
struct ResponseObservation<'a> {
    harness: Option<&'a SmartHarness>,
    bytes: Vec<u8>,
    pending: bool,
}

impl<'a> ResponseObservation<'a> {
    fn new(harness: Option<&'a SmartHarness>) -> Self {
        Self {
            harness,
            bytes: Vec::new(),
            pending: true,
        }
    }

    fn finish(mut self) -> anyhow::Result<Vec<u8>> {
        self.pending = false;
        if let Some(harness) = self.harness {
            harness.observe(&self.bytes)?;
        }
        Ok(std::mem::take(&mut self.bytes))
    }
}

impl Drop for ResponseObservation<'_> {
    fn drop(&mut self) {
        if self.pending {
            if let Some(harness) = self.harness {
                harness.observe_on_drop(&self.bytes);
            }
        }
    }
}

/// Observe exact response bytes once, then choose the provider's decoder.
pub(crate) async fn response_with_decoder(
    response: reqwest::Response,
    harness: Option<&SmartHarness>,
    prefix: &str,
    decode: fn(&[u8]) -> anyhow::Result<Value>,
) -> anyhow::Result<Value> {
    let status = response.status();
    let mut observation = ResponseObservation::new(harness);
    let read_error = crate::retry::read_response_bytes_into(response, &mut observation.bytes).await;
    let bytes = observation.finish()?;
    // A parsed SSE error envelope may arrive under HTTP 200 before the socket
    // closes. Inspect that evidence before preferring the body-read failure.
    // Successful content quoting the same text must remain ordinary content.
    let decoded = status.is_success().then(|| decode(&bytes));
    let provider_rejection = decoded.as_ref().is_some_and(|result| {
        result
            .as_ref()
            .is_err_and(super::openai_sse::is_provider_error)
    });
    let overflow = if let Some(Err(error)) = &decoded {
        crate::retry::classify(error) == crate::retry::Retryability::ContextExceeded
    } else {
        !status.is_success()
            && super::cw_overflow::is_context_overflow(&String::from_utf8_lossy(&bytes))
    };
    let read_diagnostic = read_error
        .as_ref()
        .map(|error| format!("; response body read failed: {error}"))
        .unwrap_or_default();
    if let Some(error) = read_error {
        if status.is_success() && !overflow && !provider_rejection {
            if let Some(harness) = harness {
                harness.provider_failure(&error.to_string())?;
            }
            return Err(super::observability::DispatchError::response_read(
                "request failed reading response",
                error,
            )
            .into());
        }
    }
    if !status.is_success() {
        if let Some(harness) = harness.filter(|_| !overflow) {
            harness.provider_failure(&format!("{prefix} {status}"))?;
        }
        return Err(super::observability::DispatchError::http_status(format!(
            "{prefix} {status}: {}{read_diagnostic}",
            String::from_utf8_lossy(&bytes),
        ))
        .into());
    }
    match decoded.expect("successful status selects a decoder") {
        Ok(value) => Ok(value),
        Err(error) => {
            if let Some(harness) = harness.filter(|_| !overflow) {
                harness.provider_failure(&error.to_string())?;
            }
            if read_diagnostic.is_empty() {
                Err(error)
            } else {
                let diagnostic = format!("{error:#}{read_diagnostic}");
                Err(error.context(diagnostic))
            }
        }
    }
}

/// Per-message rounding can hide required elision when converting tokens to bytes.
/// Force projection without changing the separate token admission budget.
pub(super) fn projection_byte_budget(
    messages: &[Value],
    token_budget: usize,
    est: crate::tokens::TokenEstimation,
) -> anyhow::Result<usize> {
    let max_bytes = est.chars_for_tokens(token_budget);
    if super::estimate_tokens(messages, est) > token_budget {
        Ok(max_bytes.min(serde_json::to_vec(messages)?.len().saturating_sub(1)))
    } else {
        Ok(max_bytes)
    }
}

/// Reuse the existing pressure trigger and wire bridges; smart mode elides
/// through the verified frame instead of running the legacy summarizer.
pub(super) async fn compress(
    req: super::compress::CompressRequest<'_>,
    summarizer: Option<&SummarizeFn>,
    state: &mut super::CompressState,
    harness: Option<&SmartHarness>,
) -> anyhow::Result<super::compress::CompressOutcome> {
    let Some(harness) = harness else {
        return Ok(super::compress::compress(req, summarizer, state).await);
    };
    let messages = harness
        .project(
            req.messages,
            projection_byte_budget(req.messages, req.budget, req.est)?,
        )
        .await?;
    let tokens_before = super::estimate_tokens(req.messages, req.est);
    let tokens_after = super::estimate_tokens(&messages, req.est);
    anyhow::ensure!(
        tokens_after <= req.budget,
        "smart harness projection exceeds the input budget"
    );
    let fired = messages != req.messages;
    Ok(super::compress::CompressOutcome {
        messages,
        action: if fired {
            super::compress::CompressAction::Pruned
        } else {
            super::compress::CompressAction::Fit
        },
        refusal: None,
        fired,
        tokens_before,
        tokens_after,
        notice: None,
    })
}

pub(super) fn push_tool_message(
    messages: &mut Vec<Value>,
    message: Value,
    harness: Option<&SmartHarness>,
    host_supplied: bool,
    _name: &str,
) -> anyhow::Result<()> {
    if let Some(harness) = harness {
        if host_supplied {
            harness.host_envelope(&message)?;
        }
    }
    messages.push(message);
    Ok(())
}

pub(crate) fn tool_result(
    name: &str,
    result: String,
    offload: bool,
    spill: Option<&dyn super::content_spill::SpillStore>,
    disclosure: Option<&crate::ocap::DisclosureFilter>,
    invocation: Option<&ToolInvocation<'_>>,
) -> anyhow::Result<String> {
    match invocation {
        Some(invocation) => invocation.model_text(),
        None => Ok(super::maybe_offload_tool_result(
            name, result, offload, spill, disclosure,
        )),
    }
}

/// Parse the strict tool-less classifier protocol; prose and aliases are failures.
pub fn parse_verdict(text: &str) -> Option<&'static str> {
    match serde_json::from_str::<String>(text).ok()?.as_str() {
        "answer" => Some("answer"),
        "narration" => Some("narration"),
        "question" => Some("question"),
        _ => None,
    }
}

#[cfg(test)]
#[path = "smart_harness_tests/context_exceeded.rs"]
mod context_exceeded_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn harness(replies: &[&str], settings: AdjudicationSettings) -> SmartHarness {
        let replies = Arc::new(Mutex::new(
            replies
                .iter()
                .map(|s| s.to_string())
                .collect::<std::collections::VecDeque<_>>(),
        ));
        SmartHarness::new(
            Session::new(Default::default()).unwrap(),
            Arc::new(move |_| {
                let reply = replies.lock().unwrap().pop_front().unwrap();
                Box::pin(async move { Ok(reply) })
            }),
            settings,
        )
        .unwrap()
    }

    fn observation(h: &SmartHarness, text: &str) {
        h.request(
            &serde_json::json!({"messages":[{"role":"user","content":"What is two plus one?"}]}),
            "openai",
        )
        .unwrap();
        h.observe(text.as_bytes()).unwrap();
    }

    #[test]
    fn verdict_protocol_accepts_only_an_exact_class() {
        assert_eq!(super::parse_verdict("\"answer\""), Some("answer"));
        assert_eq!(super::parse_verdict(" \"question\"\n"), Some("question"));
        for invalid in [
            "answer",
            "Here: \"answer\"",
            "\"completed\"",
            "[\"answer\"]",
        ] {
            assert_eq!(super::parse_verdict(invalid), None);
        }
    }

    #[test]
    fn smart_catalog_matches_callable_scoped_tools() {
        let h = harness(&[], Default::default());
        let scope = crate::caveats::Scope::Only(std::collections::BTreeSet::new());
        let tools = super::super::tools::merged_tool_definitions(
            &super::super::mcp::NoMcp,
            false,
            false,
            false,
            Some(&scope),
            true,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        let legacy = tools.clone();
        let smart = advertise(tools, Some(&h));
        let definition = |catalog: &Value, name: &str| {
            catalog
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["function"]["name"] == name)
                .cloned()
        };
        assert!(definition(&smart, "crew").is_none());
        assert!(definition(&smart, "find").is_none());
        assert!(definition(&smart, "compose_roster").is_some());
        assert!(definition(&smart, "re_read").is_some());
        assert_eq!(definition(&smart, "git"), definition(&legacy, "git"));
        let command = definition(&smart, "run_command").unwrap();
        let description = command["function"]["description"].as_str().unwrap();
        assert!(description.contains("Scoped shell Git reads and staging"));
        assert!(description.contains("Git commits are refused"));
        assert_eq!(advertise(legacy.clone(), None), legacy);
    }

    #[test]
    fn admitted_tool_batches_settle_the_original_observation_without_a_verdict() {
        let h = harness(&[], Default::default());
        let _batch = h.fixture_tool_batch("re_read", serde_json::json!({"cid":"source"}));
        assert!(h.state().unwrap().session.pending_replies().is_empty());
    }

    #[test]
    fn cancellation_records_an_incomplete_terminal_and_reports_it_to_the_frontend() {
        let h = harness(&[], Default::default());
        observation(&h, "unadjudicated observation");
        let mut reason = None;
        assert_eq!(cancelled(Some(&h), &mut Some(&mut reason)).unwrap(), "");
        assert_eq!(reason, Some(crate::TurnEndReason::Cancelled));
        assert!(h.state().unwrap().session.pending_replies().is_empty());
        assert!(h
            .state()
            .unwrap()
            .session
            .restored_messages()
            .unwrap()
            .is_empty());
    }

    /// Grounds the response reader's future-drop path with a real socket. The
    /// write barrier proves the reader consumed the SSE prefix before
    /// cancellation; the recorded source must retain that exact wire prefix.
    #[tokio::test]
    async fn cancelled_response_reader_retains_exact_partial_wire_observation() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let config = agent_harness::SessionConfig {
            max_fetched_bytes: 32 * 1024 * 1024,
            ..Default::default()
        };
        let h = SmartHarness::new(
            Session::new(config).unwrap(),
            Arc::new(|_| Box::pin(async { unreachable!("no auxiliary call") })),
            Default::default(),
        )
        .unwrap();
        h.request(
            &serde_json::json!({"messages":[{"role":"user","content":"cancel"}]}),
            "openai",
        )
        .unwrap();

        let fragment =
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri = format!("http://{}", listener.local_addr().unwrap());
        let (delivered_tx, delivered_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 1024];
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0, "client closed before sending request headers");
                request.extend_from_slice(&chunk[..read]);
            }
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
                )
                .await
                .unwrap();
            let padding = format!(":{}\n", "x".repeat(64 * 1024 - 2));
            for _ in 0..256 {
                socket.write_all(padding.as_bytes()).await.unwrap();
            }
            delivered_tx.send(()).unwrap();
            match socket.read(&mut [0_u8; 1]).await {
                Ok(0) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    ) => {}
                other => panic!("cancelled response socket remained active: {other:?}"),
            }
        });
        let response = reqwest::Client::new().get(uri).send().await.unwrap();
        let cancel = AtomicBool::new(false);
        let mut completion = Box::pin(super::super::cancellable(
            Some(&cancel),
            response_with_decoder(
                response,
                Some(&h),
                "inference endpoint",
                decode_openai_response,
            ),
        ));
        tokio::time::timeout(Duration::from_secs(30), async {
            tokio::select! {
                _ = delivered_rx => {},
                result = &mut completion => panic!("unfinished response resolved: {result:?}"),
            }
        })
        .await
        .expect("the partial response must reach the reader");
        cancel.store(true, Ordering::Relaxed);
        assert!(completion.await.is_none());
        tokio::time::timeout(Duration::from_secs(30), server)
            .await
            .expect("the server must observe cancellation")
            .unwrap();

        {
            let mut state = h.state().unwrap();
            let reply = state
                .reply
                .expect("cancellation must retain an observed reply");
            let slice = state
                .session
                .re_read(&reply.to_string(), 0, fragment.len())
                .unwrap();
            assert_eq!(slice["text"], std::str::from_utf8(fragment).unwrap());
            assert_eq!(slice["offset"], 0);
            assert_eq!(slice["end"], fragment.len());
            assert_eq!(slice["complete"], false);
        }
        assert_eq!(
            h.state().unwrap().session.pending_replies().len(),
            1,
            "the interrupted wire response must remain pending until cancellation settles it"
        );
        let mut reason = None;
        assert_eq!(cancelled(Some(&h), &mut Some(&mut reason)).unwrap(), "");
        assert_eq!(reason, Some(crate::TurnEndReason::Cancelled));
        assert!(
            h.state().unwrap().session.pending_replies().is_empty(),
            "the interrupted observation must be settled as cancelled"
        );
    }

    #[tokio::test]
    async fn recorded_narration_is_nudged_but_a_substantive_followup_is_deliverable() {
        let h = harness(&["\"narration\"", "\"answer\""], Default::default());
        observation(&h, "Let me calculate that.");
        assert!(matches!(
            h.classify("Let me calculate that.", 1, true, None)
                .await
                .unwrap(),
            Control::Continue(_)
        ));
        observation(&h, "Three.");
        assert!(matches!(
            h.classify("Three.", 1, true, None).await.unwrap(),
            Control::Answer
        ));
    }

    #[tokio::test]
    async fn questions_wait_and_narration_never_becomes_completed_at_the_cap() {
        let h = harness(&["\"question\"", "\"narration\""], Default::default());
        observation(&h, "Which repository?");
        assert!(matches!(
            h.classify("Which repository?", 1, true, None)
                .await
                .unwrap(),
            Control::Finish {
                reason: crate::TurnEndReason::AwaitingOperator,
                ..
            }
        ));
        observation(&h, "I am finished.");
        assert!(matches!(
            h.classify("I am finished.", 1, false, None).await.unwrap(),
            Control::Finish {
                reason: crate::TurnEndReason::NarrationFinalRound,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn unrecorded_and_malformed_verdicts_cannot_complete() {
        let h = harness(&["Here is your answer: \"answer\""], Default::default());
        assert!(h.classify("done", 1, true, None).await.is_err());
        observation(&h, "done");
        let before = h.head().unwrap();
        assert!(matches!(
            h.classify("done", 1, true, None).await.unwrap(),
            Control::Finish {
                reason: crate::TurnEndReason::Failed,
                ..
            }
        ));
        assert_ne!(
            h.head().unwrap(),
            before,
            "failure is recorded after the preserved observation"
        );
    }

    #[tokio::test]
    async fn oversized_auxiliary_replies_are_retained_before_budget_rejection() {
        for navigation in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let raw = "\"answer\"";
            let h = SmartHarness::new(
                Session::open(dir.path(), Default::default()).unwrap(),
                Arc::new(move |_| Box::pin(async move { Ok(raw.to_string()) })),
                AdjudicationSettings {
                    max_output_bytes: 4,
                    ..Default::default()
                },
            )
            .unwrap();
            if navigation {
                let messages = serde_json::json!([
                    {"role":"user","content":"old source ".repeat(300)},
                    {"role":"assistant","content":"old answer"},
                    {"role":"user","content":"current task"}
                ]);
                let error = h
                    .project(messages.as_array().unwrap(), 1200)
                    .await
                    .unwrap_err();
                assert!(format!("{error:#}").contains("byte budget"));
            } else {
                observation(&h, "Three.");
                assert!(matches!(
                    h.classify("Three.", 1, true, None).await.unwrap(),
                    Control::Finish {
                        reason: crate::TurnEndReason::Failed,
                        ..
                    }
                ));
            }
            let store = agent_harness::store::FrameStore::open(dir.path()).unwrap();
            assert_eq!(
                store
                    .source(&content_addressable::RawContentId::from_content(
                        raw.as_bytes()
                    ))
                    .unwrap(),
                raw.as_bytes()
            );
        }
    }

    #[tokio::test]
    async fn auxiliary_system_instruction_shares_the_input_byte_budget() {
        let h = harness(
            &["unused"],
            AdjudicationSettings {
                system_instruction: "system".into(),
                max_input_bytes: 8,
                ..Default::default()
            },
        );
        let error = h.complete_bounded("payload".into()).await.unwrap_err();
        assert!(error.to_string().contains("input exceeds its byte budget"));
        assert_eq!(h.state().unwrap().calls, 0, "no inference before admission");
        assert!(AdjudicationSettings::default()
            .system_instruction
            .is_empty());
    }

    #[tokio::test]
    async fn auxiliary_timeout_and_cancellation_are_bounded_and_recorded() {
        let settings = AdjudicationSettings {
            timeout_ms: 1,
            ..Default::default()
        };
        let h = SmartHarness::new(
            Session::new(Default::default()).unwrap(),
            Arc::new(|_| Box::pin(std::future::pending())),
            settings,
        )
        .unwrap();
        observation(&h, "pending");
        assert!(matches!(
            h.classify("pending", 1, true, None).await.unwrap(),
            Control::Finish {
                reason: crate::TurnEndReason::Failed,
                ..
            }
        ));
        observation(&h, "pending");
        let cancel = std::sync::atomic::AtomicBool::new(true);
        assert!(matches!(
            h.classify("pending", 1, true, Some(&cancel)).await.unwrap(),
            Control::Finish {
                reason: crate::TurnEndReason::Cancelled,
                ..
            }
        ));
    }

    /// Grounds the cancellation wrapper's future-drop behavior in a reopened
    /// frame store: a cancelled auxiliary request must retain its causal failure.
    #[tokio::test]
    async fn cancelled_initial_navigation_retains_a_request_linked_failure() {
        use agent_harness::forensics::inspect_from_store;
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let callback_started = Arc::clone(&started);
        let h = SmartHarness::new(
            Session::open(dir.path(), Default::default()).unwrap(),
            Arc::new(move |_| {
                callback_started.notify_one();
                Box::pin(std::future::pending())
            }),
            Default::default(),
        )
        .unwrap();
        let messages = serde_json::json!([
            {"role":"user","content":"old source ".repeat(300)},
            {"role":"assistant","content":"old answer"},
            {"role":"user","content":"current task"}
        ]);
        let cancel = AtomicBool::new(false);
        let mut navigation = Box::pin(super::super::cancellable(
            Some(&cancel),
            h.project(messages.as_array().unwrap(), 1200),
        ));
        tokio::select! {
            _ = started.notified() => {},
            _ = &mut navigation => panic!("navigation must wait for its auxiliary reply"),
        }
        let store = agent_harness::store::FrameStore::open(dir.path()).unwrap();
        let request = inspect_from_store(&store, h.head().unwrap(), Default::default())
            .unwrap()
            .unwrap();
        let request: ContentId = request.references[0].cid.parse().unwrap();
        cancel.store(true, Ordering::Relaxed);
        assert!(navigation.await.is_none());
        let mut reason = None;
        cancelled(Some(&h), &mut Some(&mut reason)).unwrap();
        assert_eq!(reason, Some(crate::TurnEndReason::Cancelled));
        assert!(h.state().unwrap().request.is_none(), "no primary dispatch");
        assert!(h.state().unwrap().reply.is_none());

        let failure = inspect_from_store(&store, h.head().unwrap(), Default::default())
            .unwrap()
            .unwrap();
        let failure = inspect_from_store(
            &store,
            failure.references[0].cid.parse().unwrap(),
            Default::default(),
        )
        .unwrap()
        .unwrap();
        assert!(
            failure.parents.contains(&request),
            "failure must name its request"
        );
        let payload = failure
            .references
            .iter()
            .find(|reference| reference.relation == "payload")
            .unwrap();
        let payload: Value =
            serde_json::from_slice(&store.source(&payload.cid.parse().unwrap()).unwrap()).unwrap();
        assert!(payload["failure"].as_str().unwrap().contains("cancelled"));
        let head = h.head().unwrap();
        drop(h);
        Session::restore(dir.path(), head, "local-session").unwrap();
    }

    /// Grounds cancellation error propagation against a real failed checkpoint
    /// publication; a callback drop must not report a clean cancellation.
    #[tokio::test]
    async fn cancelled_navigation_surfaces_checkpoint_failure() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let directory = dir.path().to_path_buf();
        let cancel = Arc::new(AtomicBool::new(false));
        let callback_cancel = Arc::clone(&cancel);
        let h = SmartHarness::new(
            Session::open(dir.path(), Default::default()).unwrap(),
            Arc::new(move |_| {
                std::fs::rename(directory.join("heads"), directory.join("retained-heads")).unwrap();
                std::fs::write(directory.join("heads"), b"blocked checkpoint directory").unwrap();
                callback_cancel.store(true, Ordering::Relaxed);
                Box::pin(std::future::pending())
            }),
            Default::default(),
        )
        .unwrap();
        let messages = serde_json::json!([
            {"role":"user","content":"old source ".repeat(300)},
            {"role":"assistant","content":"old answer"},
            {"role":"user","content":"current task"}
        ]);
        assert!(super::super::cancellable(
            Some(&cancel),
            h.project(messages.as_array().unwrap(), 1200),
        )
        .await
        .is_none());
        let mut reason = None;
        let error = cancelled(Some(&h), &mut Some(&mut reason)).unwrap_err();
        assert!(error
            .to_string()
            .contains("cancellation could not be recorded"));
        assert!(
            reason.is_none(),
            "failed persistence is not clean cancellation"
        );
        assert!(
            h.head().is_err(),
            "failed storage cannot expose a new checkpoint"
        );
    }

    #[tokio::test]
    async fn malformed_navigation_never_falls_back_to_a_deterministic_selection() {
        let h = harness(&["keep everything please"], Default::default());
        let messages = serde_json::json!([
            {"role":"system","content":"test"},
            {"role":"user","content":"old source ".repeat(300)},
            {"role":"assistant","content":"old answer"},
            {"role":"user","content":"current request"}
        ]);
        let error = h
            .project(messages.as_array().unwrap(), 1200)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("AdjudicationFailure"),
            "invalid auxiliary output must stop navigation: {error}"
        );
    }

    #[tokio::test]
    async fn accepted_navigation_keeps_pins_and_retains_retrievable_source_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let h = SmartHarness::new(
            Session::open(dir.path(), Default::default()).unwrap(),
            Arc::new(|prompt| {
                let catalog: Value = serde_json::from_str(prompt.lines().last().unwrap()).unwrap();
                let mut selected = catalog["candidates"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|card| card["required"] == true)
                    .map(|card| card["cid"].clone())
                    .collect::<Vec<_>>();
                selected.extend(catalog["host_pinned"].as_array().unwrap().iter().cloned());
                Box::pin(async move { Ok(serde_json::to_string(&selected).unwrap()) })
            }),
            Default::default(),
        )
        .unwrap();
        let messages = serde_json::json!([
            {"role":"system","content":"system pin"},
            {"role":"user","content":"retained original source ".repeat(200)},
            {"role":"assistant","content":"old answer"},
            {"role":"user","content":"current request"}
        ]);
        let projected = h.project(messages.as_array().unwrap(), 1200).await.unwrap();
        assert_eq!(projected[0], messages[0]);
        assert_eq!(projected.last().unwrap(), &messages[3]);
        assert!(serde_json::to_vec(&projected).unwrap().len() <= 1200);
        let hint = projected[1]["content"].as_str().unwrap();
        let cid = hint
            .split("frame ")
            .nth(1)
            .unwrap()
            .split('.')
            .next()
            .unwrap();
        let head = h.head().unwrap();
        drop(h);
        let restored = Session::restore(dir.path(), head, "local-session").unwrap();
        let restored = SmartHarness::new(
            restored,
            Arc::new(|_| panic!("re-read never uses inference")),
            Default::default(),
        )
        .unwrap();
        let read: Value = serde_json::from_str(
            &restored
                .read(&serde_json::json!({"cid":cid,"max_bytes":128}))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(read["complete"], false);
        assert_eq!(read["next_offset"], 128);
        assert!(read["text"]
            .as_str()
            .unwrap()
            .contains("retained original source"));
    }

    #[test]
    fn later_turns_restore_rich_tool_history_before_appending_the_new_operator_task() {
        let h = harness(&[], Default::default());
        let history = serde_json::json!([
            {"role":"system","content":"old system"},
            {"role":"user","content":"first task"},
            {"type":"function_call","call_id":"call_1","name":"read_file","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_1","output":"exact source"}
        ]);
        h.record_messages(history.as_array().unwrap()).unwrap();
        h.start_turn().unwrap();
        let current = serde_json::json!([
            {"role":"system","content":"current system"},
            {"role":"user","content":"first task"},
            {"role":"assistant","content":"lossy display transcript"},
            {"role":"user","content":"next task"}
        ]);
        let restored = h
            .initial_messages(current.as_array().unwrap().clone())
            .unwrap();
        assert_eq!(restored[0], current[0]);
        assert_eq!(&restored[1..4], &history.as_array().unwrap()[1..]);
        assert_eq!(restored.last().unwrap(), &current[3]);
        let (_, input) = crate::responses_wire::build_responses_input(&restored);
        assert_eq!(input[1], history[2]);
        assert_eq!(input[2], history[3]);
    }

    #[test]
    fn offloaded_tool_source_survives_restart_without_a_live_spill_store() {
        let dir = tempfile::tempdir().unwrap();
        let h = SmartHarness::new(
            Session::open(dir.path(), Default::default()).unwrap(),
            Arc::new(|_| panic!("no inference")),
            Default::default(),
        )
        .unwrap();
        let batch = h.fixture_tool_batch("run_command", serde_json::json!({"command":"fixture"}));
        let invocation = batch.start(0, None).unwrap();
        let store = super::super::content_spill::SessionSpillStore::new([1; 16]);
        let full = "retained full command output\n".repeat(1000);
        let (handle, _) = super::super::content_spill::store_redacted_full(
            &full,
            Some("run_command".into()),
            &store,
        );
        let display = format!(
            "head ... tail\n{}\nerror: command exited 7",
            super::super::content_spill::tool_output_retrieval_hint(&handle.unwrap())
        );
        invocation.observe(&display, Some(&store)).unwrap();
        let rendered = tool_result(
            "run_command",
            display,
            false,
            Some(&store),
            None,
            Some(&invocation),
        )
        .unwrap();
        push_tool_return(
            &mut Vec::new(),
            serde_json::json!({"role":"tool","tool_call_id":"fixture_call","content":rendered}),
            Some(&invocation),
        )
        .unwrap();
        drop(invocation);
        drop(batch);
        let payload: Value = serde_json::from_str(&rendered).expect("durable first-slice envelope");
        assert!(payload["display"].as_str().unwrap().contains("exited 7"));
        assert!(!rendered.contains("spill:"));
        let source = payload["sources"][0]["source_cid"].clone();
        assert_eq!(payload["sources"][0]["slice"]["complete"], false);
        let head = h.head().unwrap();
        drop(h);
        let restored = Session::restore(dir.path(), head, "local-session").unwrap();
        let restored = SmartHarness::new(
            restored,
            Arc::new(|_| panic!("no inference")),
            Default::default(),
        )
        .unwrap();
        let tail: Value = serde_json::from_str(
            &restored
                .read(&serde_json::json!({"cid":source,"offset":full.len()-20,"max_bytes":20}))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(tail["complete"], true);
        assert_eq!(tail["text"], &full[full.len() - 20..]);
    }
}
