//! A **non-blocking turn driver** around [`chat_complete`](super::chat_complete)
//! (issue #308 — the cowork foundation).
//!
//! [`chat_complete`] is `async` and, inside `run_chat`, is pumped by a blocking
//! `rustyline` REPL (`block_in_place` + `block_on`). A downstream `ratatui`
//! app cannot block its event loop like that: it must poll input, redraw, and
//! poll the turn's progress every frame. This module is that seam — a struct an
//! external `crossterm` event loop drives without ever calling `run_chat`:
//!
//! ```text
//! loop {                                 // the consumer's crossterm loop
//!     if event::poll(..) { /* read key, maybe driver.submit(line) */ }
//!     match driver.poll() {              // non-blocking
//!         TurnStatus::Completed { .. } => { /* redraw transcript */ }
//!         TurnStatus::Running          => { /* show a spinner */ }
//!         _ => {}
//!     }
//!     // turn driver.transcript() into rows via transcript_lines(.., width)
//!     terminal.draw(|f| draw(transcript_lines(driver.transcript(), f.area().width as usize)));
//! }
//! ```
//!
//! ## How it spans the async boundary
//!
//! [`ChatCtx`](super::ChatCtx) is borrow-heavy — most of its fields are
//! references with the caller's lifetime, several are `&mut dyn` session
//! handles (note sink, recall source, permission gate, …). Those trait objects
//! aren't `Send`, so `chat_complete`'s future isn't `Send` either and cannot go
//! through `tokio::spawn`. So the driver owns a [`TurnDriverConfig`] of plain
//! owned values and, on [`submit`](TurnDriver::submit), launches a **dedicated
//! OS thread** running its own current-thread tokio runtime. That thread builds
//! a **headless** `ChatCtx` from a clone of the config plus a snapshot of the
//! transcript, `block_on`s one turn against [`NoMcp`] (raced against a cancel
//! signal), and sends the result back over a [`oneshot`] channel. The
//! session-bound `Option` fields (`note_sink`, `recall_source`, `summarizer`,
//! `compress_state`, …) are all `None` here: a cowork turn is a clean drive,
//! exactly like the ACP worker / `newt-eval` headless callers. Compression and
//! budget logic are untouched — the driver wraps `chat_complete`, it does not
//! reach inside it.
//!
//! Running on its own thread also means the driver does **not** require the
//! consumer to call from inside a tokio runtime — a plain `crossterm` loop can
//! own a `TurnDriver` with no async scaffolding of its own.
//!
//! ## Minimal by design
//!
//! `submit` → `poll` → `cancel`, plus `submit_observation` to fold a
//! [`ShellObservation`] into the next turn's context. That is the whole
//! surface; richer wiring (live MCP tools, a session note store) stays with the
//! downstream consumer, which can build a full `ChatCtx` itself if it needs
//! one.

use std::thread::JoinHandle;

use tokio::sync::oneshot;

use std::sync::Arc;

use super::observation::ShellObservation;
use super::{
    chat_complete, ChatCtx, CodeSearch, CrewRunner, Embedder, NoMcp, PromptDisposition,
    SemanticIndex,
};
use crate::{BackendKind, CompactionTriggerPolicy, MemMessage, Role, TokenUsage};

/// Owned, cloneable retrieval handle for the headless driver — the `'static`
/// counterpart of the borrow-heavy [`CodeSearch`] `ChatCtx` field (#1280). A
/// crew-leaf / cowork consumer that has an embedder + a built session index
/// hands them in here (shared `Arc`s, so cloning the config per turn is cheap);
/// [`run_one_turn`] borrows them into the turn's `CodeSearch`. Absent ⇒ the
/// `code_search` tool is simply not advertised, exactly like today.
#[derive(Clone)]
pub struct HeadlessCodeSearch {
    /// The query embedder (the same trait the live TUI loop uses).
    pub embedder: Arc<dyn Embedder>,
    /// The session semantic index the embedder searches.
    pub index: Arc<dyn SemanticIndex>,
    /// Default number of chunks returned per search.
    pub top_k: usize,
}

impl std::fmt::Debug for HeadlessCodeSearch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The trait objects are not `Debug`; surface only the scalar knob so the
        // derived `Debug` on `TurnDriverConfig` still compiles.
        f.debug_struct("HeadlessCodeSearch")
            .field("top_k", &self.top_k)
            .finish_non_exhaustive()
    }
}

/// Owned, `'static` configuration for one [`TurnDriver`] — the cloneable
/// counterpart of the borrow-heavy [`ChatCtx`]. The driver clones it into each
/// spawned turn task so nothing borrows across the async boundary.
///
/// The fields mirror the inference-relevant subset of [`ChatCtx`]; the
/// session-bound `&mut` handles (note sink, recall, permission gate, summarizer,
/// compress state) are intentionally absent — a driven cowork turn is headless.
#[derive(Debug, Clone)]
pub struct TurnDriverConfig {
    /// Inference endpoint base URL.
    pub url: String,
    /// Model name.
    pub model: String,
    /// Wire protocol of the backend.
    pub kind: BackendKind,
    /// Bearer token for authenticated OpenAI-compatible endpoints.
    pub api_key: Option<String>,
    /// Chat Completions extensions explicitly accepted by the endpoint.
    pub chat_completions_capability: crate::model_card::ChatCompletionsCapability,
    /// Whether the active backend accepts replayed assistant reasoning.
    pub reasoning_replay_scope: crate::model_card::ReasoningReplayScope,
    /// Whether the active model streams a lone leading `</think>` closer, so
    /// the stream filter must start inside the reasoning block (#528).
    ///
    /// Resolved from the backend's capability layers (inline declaration,
    /// and a named `card =` binding via `ResolvedCapabilities`) — never from
    /// the model name.
    pub emits_leading_reasoning: bool,
    /// Absolute workspace path the tool loop runs against.
    pub workspace: String,
    /// Permission caveats enforced for this turn's tool calls.
    pub caveats: crate::caveats::Caveats,
    /// Maximum tool-call rounds before a forced final completion.
    pub max_tool_rounds: usize,
    /// Max narrate-then-stop rescue nudges per turn (see
    /// `[tui] narration_nudge_cap`); cowork default 1 keeps the historical
    /// one-shot rescue.
    pub narration_nudge_cap: usize,
    /// Additional progress-aware rounds after `max_tool_rounds`; `0` makes the
    /// normal cap hard.
    pub workflow_grace_rounds: usize,
    /// Legacy line limit for pre-execution tool previews.
    pub tool_output_lines: usize,
    /// Full model context window. Sent as Ollama `options.num_ctx`; on the
    /// OpenAI-compatible path it stays local and provides the authoritative
    /// input ceiling after percentage and generation-output reserves.
    pub num_ctx: Option<u32>,
    /// TCP connect timeout.
    pub connect_timeout_secs: u64,
    /// Total inference timeout.
    pub inference_timeout_secs: u64,
    /// Message count at which the in-flight conversation is trimmed mid-turn.
    pub mid_loop_trim_threshold: usize,
    /// Whether a known authoritative input ceiling suppresses a count-only
    /// compaction. Headless callers default to the same safe policy as the
    /// interactive harness.
    pub compaction_trigger_policy: CompactionTriggerPolicy,
    /// Token threshold that also triggers a mid-loop trim. `None` disables it.
    pub mid_loop_trim_tokens: Option<usize>,
    /// Highest input-token count the model has accepted (pre-send budget gate).
    pub max_ok_input: Option<u32>,
    /// Shell command run after every successful write for ground-truth feedback.
    pub build_check_cmd: Option<String>,
    /// Empirically safe context size used to detect likely overflow.
    pub safe_context: Option<u32>,
    /// #727: percent of `num_ctx` used as the input-token ceiling (default 80).
    pub input_ceiling_pct: u32,
    /// #727: remaining-budget percent below which the loop nudges (default 15).
    pub low_budget_pct: usize,
    /// Cheap request-token estimator configured under `[context.estimation]`.
    pub estimation: crate::tokens::TokenEstimation,
    /// Minimum summarizer input cap configured under `[context]`.
    pub summary_input_cap_floor_chars: usize,
    /// Context-management preset configured under `[context] manager`. Headless
    /// callers get the same preset the interactive harness resolves, so
    /// `manager = "append-only"` in a config file governs `newt solve` too —
    /// otherwise the setting would silently mean nothing off the TUI.
    pub context_manager: crate::ContextManager,
    /// #1280: optional semantic retrieval for the headless turn. `Some` ⇒ the
    /// `code_search` tool is advertised and executable against the supplied
    /// index; `None` (the default) ⇒ absent, preserving today's behavior.
    pub code_search: Option<HeadlessCodeSearch>,
}

impl TurnDriverConfig {
    /// Construct a config with sensible cowork defaults; only the endpoint, the
    /// model, the backend kind, and the workspace are required. Caveats default
    /// to [`Caveats::top`](crate::caveats::Caveats::top) — the downstream
    /// consumer is expected to narrow them.
    pub fn new(
        url: impl Into<String>,
        model: impl Into<String>,
        kind: BackendKind,
        workspace: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            model: model.into(),
            kind,
            api_key: None,
            chat_completions_capability: Default::default(),
            reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
            emits_leading_reasoning: false,
            workspace: workspace.into(),
            caveats: crate::caveats::Caveats::top(),
            max_tool_rounds: 40,
            narration_nudge_cap: 1,
            workflow_grace_rounds: 5,
            tool_output_lines: 20,
            num_ctx: None,
            connect_timeout_secs: 5,
            inference_timeout_secs: 120,
            mid_loop_trim_threshold: 40,
            compaction_trigger_policy: CompactionTriggerPolicy::HeadroomAware,
            mid_loop_trim_tokens: None,
            max_ok_input: None,
            build_check_cmd: None,
            safe_context: None,
            input_ceiling_pct: 80,
            low_budget_pct: 15,
            estimation: crate::tokens::TokenEstimation::default(),
            summary_input_cap_floor_chars: 8_192,
            context_manager: crate::ContextManager::default(),
            code_search: None,
        }
    }

    /// #1280: enable semantic `code_search` for driven turns. The consumer owns
    /// the embedder + index (built once, shared via `Arc`); the driver borrows
    /// them into each turn's `ChatCtx`. Mirrors the live TUI loop, which offers
    /// the tool only when semantic is on.
    #[must_use]
    pub fn with_code_search(
        mut self,
        embedder: Arc<dyn Embedder>,
        index: Arc<dyn SemanticIndex>,
        top_k: usize,
    ) -> Self {
        self.code_search = Some(HeadlessCodeSearch {
            embedder,
            index,
            top_k,
        });
        self
    }
}

/// The outcome of one completed turn.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    /// The model's reply text.
    pub reply: String,
    /// Whether the reply was streamed token-by-token by the loop (it prints to
    /// stdout when `color` is on; the driver always runs headless, so this is
    /// informational).
    pub was_streamed: bool,
    /// Token usage for the turn, when the backend reported it.
    pub usage: Option<TokenUsage>,
    /// Count of hallucinated (non-existent) tool calls the loop saw.
    pub hallucinations: u32,
    /// Per-tool-call trace of the turn (name, args-digest, ok, duration) — the
    /// trajectory a headless consumer (`newt solve` / Terminal-Bench) serializes
    /// to reason about what the agent actually did. Empty when the loop produced
    /// no tool calls.
    pub tool_events: Vec<crate::ToolEvent>,
    /// Why the turn ended (genuine completion vs round-cap vs empty), when the
    /// loop set it.
    pub end_reason: Option<crate::TurnEndReason>,
    /// The inference/loop error, when the turn failed part-way. `Some` means the
    /// turn did NOT complete cleanly — but `reply`/`usage` may be empty while
    /// `tool_events` still holds whatever the agent did *before* the error (an
    /// infrastructure failure must not erase the partial trajectory).
    pub error: Option<String>,
    /// Structural class of `error` (W0 #1511), read from the TYPED dispatch
    /// error chain — never from the message text. `Some(Harness)` when an
    /// error carried no dispatch classification at all (fail-closed: an
    /// unattributed failure is ours, not the model's). `None` iff `error` is.
    pub error_class: Option<crate::agentic::observability::ErrorClass>,
    /// The `model` field the backend reported in its response body, when it
    /// did — what was ACTUALLY served (the solve contract's `effective_model`
    /// source), as opposed to what we asked for.
    pub served_model: Option<String>,
    /// Per-round tool-call parse signals (ADR #1506 §5): recovered dialects
    /// and content-with-no-parseable-call rounds, in round order.
    pub parse_signals: Vec<crate::agentic::observability::ParseSignal>,
    /// Output-behavior signals, including bounded reasoning-overflow recovery.
    pub behavior_signals: Vec<crate::agentic::observability::BehaviorSignal>,
}

/// Non-blocking snapshot of the driver's state, returned by
/// [`TurnDriver::poll`].
#[derive(Debug)]
pub enum TurnStatus {
    /// No turn in flight (nothing submitted, or the last result was drained).
    Idle,
    /// A turn is running; the result is not ready yet.
    Running,
    /// A turn finished successfully. Returned **once**; the reply has already
    /// been appended to the transcript as an assistant message.
    Completed(TurnOutcome),
    /// A turn failed (transport error, dispatch error, …). Returned **once**.
    Failed(String),
}

/// Internal in-flight turn handle: the worker thread, the channel it reports
/// on, and the cancel signal that races its `block_on`.
struct InFlight {
    handle: JoinHandle<()>,
    rx: oneshot::Receiver<Result<TurnOutcome, String>>,
    cancel_tx: Option<oneshot::Sender<()>>,
}

/// Runtime choices captured for one driver independently of its source-stable
/// inference configuration. Each submitted turn clones this posture onto the
/// dedicated worker thread.
#[derive(Clone)]
struct HeadlessRuntimePosture {
    cognition: Option<crate::role_profile::Cognition>,
    tenacity: crate::tenacity::Tenacity,
    crew_runner: Option<Arc<dyn CrewRunner>>,
}

impl HeadlessRuntimePosture {
    fn capture() -> Self {
        Self {
            cognition: crate::cognition::effective_cognition(),
            tenacity: crate::tenacity::effective_tenacity(),
            crew_runner: None,
        }
    }
}

/// A pumpable, non-blocking driver for one agentic turn at a time.
///
/// Owns the running transcript and at most one in-flight turn. The external
/// event loop [`submit`](Self::submit)s input (or
/// [`submit_observation`](Self::submit_observation)s shell activity),
/// [`poll`](Self::poll)s for completion every frame, and may
/// [`cancel`](Self::cancel) the in-flight turn.
pub struct TurnDriver {
    config: TurnDriverConfig,
    runtime: HeadlessRuntimePosture,
    transcript: Vec<MemMessage>,
    in_flight: Option<InFlight>,
}

impl TurnDriver {
    /// Build a driver with an empty transcript.
    pub fn new(config: TurnDriverConfig) -> Self {
        Self {
            config,
            runtime: HeadlessRuntimePosture::capture(),
            transcript: Vec::new(),
            in_flight: None,
        }
    }

    /// Build a driver seeded with an existing transcript (e.g. a system prompt
    /// plus prior turns the consumer already assembled).
    pub fn with_transcript(config: TurnDriverConfig, transcript: Vec<MemMessage>) -> Self {
        Self {
            config,
            runtime: HeadlessRuntimePosture::capture(),
            transcript,
            in_flight: None,
        }
    }

    /// The current transcript — every message the model has seen plus the
    /// replies it produced. Feed it to
    /// [`transcript_lines`](super::transcript_lines) for renderer-agnostic,
    /// width-wrapped rows, or render it however the consumer likes.
    pub fn transcript(&self) -> &[MemMessage] {
        &self.transcript
    }

    /// Whether a turn is currently in flight.
    pub fn is_running(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Pin the cognition projection used by every turn this driver submits.
    /// The default is captured from the process posture when the driver is
    /// constructed, before its dedicated turn thread is spawned.
    #[must_use]
    pub fn with_cognition(mut self, cognition: Option<crate::role_profile::Cognition>) -> Self {
        self.runtime.cognition = cognition;
        self
    }

    /// Pin the tenacity used by every resolver read in this driver's turns.
    /// The default is captured from the process posture at construction.
    #[must_use]
    pub fn with_tenacity(mut self, tenacity: crate::tenacity::Tenacity) -> Self {
        self.runtime.tenacity = tenacity;
        self
    }

    /// Enable the real crew/team surface for driven turns. The runner is owned
    /// through an `Arc` because every submitted turn executes on a dedicated
    /// OS thread.
    #[must_use]
    pub fn with_crew_runner(mut self, runner: Arc<dyn CrewRunner>) -> Self {
        self.runtime.crew_runner = Some(runner);
        self
    }

    /// Whether driven turns will advertise and execute crew tools.
    #[must_use]
    pub fn has_crew_runner(&self) -> bool {
        self.runtime.crew_runner.is_some()
    }

    /// Append a [`ShellObservation`] to the transcript so it becomes part of the
    /// **next** turn's context. The observation is redacted and framed by
    /// [`ShellObservation::into_mem_message`] — credentials in shell output are
    /// scrubbed before they enter the transcript. This does **not** start a
    /// turn on its own; a real human message ([`submit`](Self::submit)) drives
    /// the loop, with the accumulated observations already in context.
    pub fn submit_observation(&mut self, obs: ShellObservation) {
        self.transcript.push(obs.into_mem_message());
    }

    /// Submit a human message and start a turn.
    ///
    /// Appends the message as a `User` turn, snapshots the transcript, and
    /// spawns a task that runs one [`chat_complete`] against the configured
    /// backend. Returns an error (without starting anything) if a turn is
    /// already in flight — the driver runs one turn at a time. Poll
    /// [`poll`](Self::poll) for the result.
    pub fn submit(&mut self, input: impl Into<String>) -> Result<(), TurnDriverError> {
        if self.in_flight.is_some() {
            return Err(TurnDriverError::Busy);
        }
        let task = input.into();
        self.transcript.push(MemMessage::user(task.clone()));
        self.spawn_turn(task);
        Ok(())
    }

    /// Launch the turn on a dedicated thread over a clone of the config + a
    /// snapshot of the transcript. The `task` string is the current user
    /// message, threaded into `ChatCtx.task` (the loop's nudges key on it).
    ///
    /// `chat_complete`'s future is `!Send` (its `ChatCtx` holds `&mut dyn`
    /// session handles), so it can't ride `tokio::spawn`. A current-thread
    /// runtime on its own OS thread keeps the non-`Send` future local; a
    /// cancel oneshot races the turn so [`cancel`](Self::cancel) returns the
    /// thread promptly instead of waiting out the inference timeout.
    fn spawn_turn(&mut self, task: String) {
        let config = self.config.clone();
        let runtime = self.runtime.clone();
        let messages = self.transcript.clone();
        let (tx, rx) = oneshot::channel();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let handle = std::thread::spawn(move || {
            // A current-thread runtime: no work-stealing, nothing to require
            // `Send`, and self-contained so the consumer needn't be in a
            // runtime itself.
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(format!("failed to start turn runtime: {e}")));
                    return;
                }
            };
            let result = rt.block_on(async move {
                tokio::select! {
                    biased;
                    // Cancellation wins the race when signalled.
                    _ = cancel_rx => Err("turn cancelled".to_string()),
                    out = run_one_turn(&config, &runtime, &messages, &task) => out,
                }
            });
            // The receiver may have been dropped (driver went away); fine.
            let _ = tx.send(result);
        });
        self.in_flight = Some(InFlight {
            handle,
            rx,
            cancel_tx: Some(cancel_tx),
        });
    }

    /// Non-blocking poll for the in-flight turn's status.
    ///
    /// - [`TurnStatus::Idle`] — nothing in flight.
    /// - [`TurnStatus::Running`] — in flight, not done.
    /// - [`TurnStatus::Completed`] — done; the reply has been appended to the
    ///   transcript as an assistant message. Returned exactly once.
    /// - [`TurnStatus::Failed`] — errored; nothing appended. Returned once.
    pub fn poll(&mut self) -> TurnStatus {
        let Some(in_flight) = self.in_flight.as_mut() else {
            return TurnStatus::Idle;
        };
        match in_flight.rx.try_recv() {
            Ok(Ok(outcome)) => {
                self.transcript
                    .push(MemMessage::assistant(outcome.reply.clone()));
                self.in_flight = None;
                TurnStatus::Completed(outcome)
            }
            Ok(Err(err)) => {
                self.in_flight = None;
                TurnStatus::Failed(err)
            }
            Err(oneshot::error::TryRecvError::Empty) => TurnStatus::Running,
            Err(oneshot::error::TryRecvError::Closed) => {
                // The task ended without sending (it was aborted, or panicked).
                self.in_flight = None;
                TurnStatus::Failed("turn task ended without a result".to_string())
            }
        }
    }

    /// Cancel the in-flight turn, if any. Signals the worker thread to abandon
    /// the turn (winning the `select!` race against `chat_complete`) and joins
    /// it so the runtime tears down before returning. The user message that
    /// started the turn stays in the transcript — the consumer can pop it for a
    /// clean retry. No-op when idle.
    ///
    /// A turn already blocked deep inside a single HTTP dispatch returns when
    /// that dispatch resolves (or times out); the cancel signal is honored at
    /// the next `select!` poll point, which in practice is between dispatches.
    pub fn cancel(&mut self) {
        if let Some(mut in_flight) = self.in_flight.take() {
            if let Some(cancel_tx) = in_flight.cancel_tx.take() {
                let _ = cancel_tx.send(());
            }
            let _ = in_flight.handle.join();
        }
    }
}

/// Run exactly one turn: assemble a headless [`ChatCtx`] from owned config +
/// messages and dispatch on the right wire protocol. Factored out of the spawn
/// closure so it is directly unit-testable against a wiremock backend.
async fn run_one_turn(
    config: &TurnDriverConfig,
    runtime: &HeadlessRuntimePosture,
    messages: &[MemMessage],
    task: &str,
) -> Result<TurnOutcome, String> {
    // Freeze every lazy `effective_tenacity()` read beneath `chat_complete`
    // (workflow state and exit_plan_mode included) to the posture captured by
    // this driver. The current-thread runtime keeps this RAII TLS guard on the
    // dedicated turn thread for the entire future.
    let _tenacity = crate::tenacity::scoped_effective_tenacity(runtime.tenacity);
    // Capture the per-tool trajectory + end reason for the outcome. The ChatCtx
    // borrows these mutably; the borrows release when `ctx` is consumed by the
    // await below, after which they move into the TurnOutcome.
    let mut tool_events: Vec<crate::ToolEvent> = Vec::new();
    let mut end_reason: Option<crate::TurnEndReason> = None;
    // W0 (#1511): served-model + parse-signal observation for the solve
    // contract — same lend-per-turn pattern as `tool_events`.
    let mut solve_obs = crate::agentic::observability::SolveObservation::default();
    // Session disclosure filter: register the live provider secret so a tool
    // result or summary that echoes it is value-redacted before it reaches the
    // model (`disclosure-gate-live-path`). Must outlive `ctx`, which borrows it.
    let session_disclosure = crate::ocap::session_disclosure_filter(config.api_key.as_deref());
    // Also install it on this turn's thread so the memory / observation /
    // compaction / spill paths (which funnel through `redact_secrets`, not the
    // explicit `disclosure` param) value-filter against the same secret.
    let _disclosure_guard = crate::ocap::scoped_session_disclosure(session_disclosure.clone());
    let ctx = ChatCtx {
        rewrites_history: config.context_manager.rewrites_history(),
        url: &config.url,
        model: &config.model,
        kind: config.kind,
        api_key: config.api_key.as_deref(),
        // Headless driver: action nudges stay on (the interactive /nudge dial
        // is a TUI-session concern, #1162).
        action_nudges: true,
        // Headless callers preserve the historical execute-capable contract
        // unless they opt into the prompt-comprehension ingress in the TUI.
        prompt_disposition: PromptDisposition::Act,
        prompt_intake: None,
        messages,
        task,
        workspace: &config.workspace,
        // The driver is headless: the loop's inline progress prints are
        // suppressed so nothing fights the consumer's ratatui frame.
        color: false,
        // Cowork renders into the consumer's frame, not the scroller — no
        // markdown ANSI here (it would fight the host UI).
        markdown: false,
        // Headless: no tool-offload (26.3) / scratchpad (26.4) — bit-for-bit.
        tool_offload: false,
        spill_store: None,
        disclosure: Some(&session_disclosure),
        completed_spill_renderer: None,
        compaction_store: None,
        scratchpad: false,
        scratchpad_store: None,
        // #1280: borrow the consumer-supplied embedder + index into this turn's
        // searcher. `Some` only when the config carries retrieval — otherwise the
        // `code_search` tool is not advertised (unchanged headless behavior).
        code_search: config.code_search.as_ref().map(|cs| CodeSearch {
            embedder: cs.embedder.as_ref(),
            index: cs.index.as_ref(),
            top_k: cs.top_k,
            steer: None,
            status: None,
        }),
        // #1285: headless driver builds no symbol index yet (a follow-up wires
        // it from the leaf's gather) — the where_is tool degrades honestly.
        where_is: None,
        nav: None,
        // Headless driver: identity exposure (advertise the full authorized
        // catalog, unchanged). The `[tool_exposure]` controller is a TUI-session
        // concern; the driver keeps bit-for-bit behaviour.
        exposure: crate::agentic::tools::ExposureSettings::default(),
        experience_store: None,
        step_ledger: None,
        caveats: &config.caveats,
        // Headless cowork carries no persona surface (FR-1 part 2, #997). The
        // driver captured the effective cognition before spawning this thread,
        // keeping the wire posture and contract observation identical.
        persona_tools: None,
        cognition: runtime.cognition,
        chat_completions_capability: config.chat_completions_capability,
        reasoning_replay_scope: config.reasoning_replay_scope,
        emits_leading_reasoning: config.emits_leading_reasoning,
        max_tool_rounds: config.max_tool_rounds,
        narration_nudge_cap: config.narration_nudge_cap,
        workflow_grace_rounds: config.workflow_grace_rounds,
        tool_output_lines: config.tool_output_lines,
        debug: false,
        trace: false,
        num_ctx: config.num_ctx,
        // #727 tunables: headless driver keeps the historical defaults
        // (80% input ceiling, 15% low-budget nudge threshold).
        input_ceiling_pct: config.input_ceiling_pct,
        low_budget_pct: config.low_budget_pct,
        connect_timeout_secs: config.connect_timeout_secs,
        inference_timeout_secs: config.inference_timeout_secs,
        mid_loop_trim_threshold: config.mid_loop_trim_threshold,
        compaction_trigger_policy: config.compaction_trigger_policy,
        mid_loop_trim_tokens: config.mid_loop_trim_tokens,
        max_ok_input: config.max_ok_input,
        build_check_cmd: config.build_check_cmd.clone(),
        safe_context: config.safe_context,
        // Numbered context recovery is stateless and shared with the TUI; the
        // remaining session-bound seams stay absent in this driven cowork.
        recover_cw_400: Some(super::recover_context_window_400),
        note_sink: None,
        note_nudge: None,
        recall_source: None,
        memory_source: None,
        summarizer: None,
        compress_state: None,
        tool_events: Some(&mut tool_events),
        phantom_reaches: None,
        end_reason: Some(&mut end_reason),
        solve_obs: Some(&mut solve_obs),
        permission_gate: None,
        // Phase 20 (spec §5): headless surfaces neither read nor write the
        // capability cache — the hook stays absent and no calibration is
        // applied, preserving today's behavior exactly.
        on_round_usage: None,
        estimate_ratio: None,
        estimation: config.estimation,
        summary_input_cap_floor_chars: config.summary_input_cap_floor_chars,
        // #307: the headless driver carries no preset clamp — exec authority is
        // whatever `config.caveats` already grants, exactly like the ACP worker
        // / newt-eval callers. A consumer enforcing a mode clamps `caveats` itself.
        exec_floor: None,
        write_ledger: None,
        attribution: None,
        // The headless driver has no interactive keyboard to interrupt from;
        // a consumer that wants cancellation drives its own ChatCtx.
        cancel: None,
        live_tool_output: None,
        git_tool: None,
        crew_runner: runtime.crew_runner.as_deref(),
        operating_mode_control: None,
        plan_mode_control: None,
        disposition_request_control: None,
        steering: None,
    };
    // NoMcp: the cowork driver advertises only the built-in tools. A consumer
    // that wants live MCP tools assembles its own ChatCtx.
    let mut mcp = NoMcp;
    // ONE dispatch owner. `chat_complete` routes openai-vs-ollama AND, for an
    // openai backend, responses-vs-chat (`api = "responses"`, surfaced via
    // NEWT_OPENAI_API). The headless driver goes through the SAME entry the
    // interactive TUI does, so the wire-format decision lives in exactly one
    // place and cannot drift. (This path used to fork its own `if kind==Openai`
    // branch that called `openai_chat_complete` directly, bypassing the
    // responses check — which is exactly why a responses-only model like
    // gpt-5.6-sol was mis-routed to /v1/chat/completions.)
    let dispatch = chat_complete(ctx, &mut mcp).await;
    // Both arms move `tool_events`/`end_reason` out (only one arm runs). On a
    // failed turn we still return `Ok` carrying the PARTIAL trajectory + the
    // error, rather than `Err` that discards what the agent already did — an
    // infrastructure failure (e.g. a context-window 500) must not misreport the
    // agent as having done nothing.
    match dispatch {
        Ok((reply, was_streamed, usage, hallucinations)) => Ok(TurnOutcome {
            reply,
            was_streamed,
            usage,
            hallucinations,
            tool_events,
            end_reason,
            error: None,
            error_class: None,
            served_model: solve_obs.served_model,
            parse_signals: solve_obs.parse_signals,
            behavior_signals: solve_obs.behavior_signals,
        }),
        Err(e) => Ok(TurnOutcome {
            reply: String::new(),
            was_streamed: false,
            usage: None,
            hallucinations: 0,
            tool_events,
            end_reason,
            // W0 (#1511): the class is read from the TYPED chain; an error
            // with no dispatch classification is OURS — harness_error,
            // fail-closed, never a guess from the message text.
            error_class: Some(
                crate::agentic::observability::error_class(&e)
                    .unwrap_or(crate::agentic::observability::ErrorClass::Harness),
            ),
            error: Some(e.to_string()),
            served_model: solve_obs.served_model,
            parse_signals: solve_obs.parse_signals,
            behavior_signals: solve_obs.behavior_signals,
        }),
    }
}

/// Errors from driving a turn.
#[derive(Debug, thiserror::Error)]
pub enum TurnDriverError {
    /// A turn is already in flight; the driver runs one at a time.
    #[error("a turn is already in flight — poll() for it or cancel() before submitting another")]
    Busy,
}

/// Roles whose messages a transcript renderer typically shows to the human. The
/// driver exposes it so a consumer's render can filter on the same set the
/// newt-tui render uses (system + tool turns are usually hidden).
pub const VISIBLE_TRANSCRIPT_ROLES: [Role; 2] = [Role::User, Role::Assistant];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentic::SessionSemanticIndex;
    use crate::caveats::{Caveats, CountBound, Scope};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    /// Ollama-shaped responder: a plain text answer (no tools), echoing back a
    /// fixed reply. Counts how many chat requests it served.
    struct PlainOllama {
        served: Arc<AtomicUsize>,
        reply: String,
    }

    impl Respond for PlainOllama {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            self.served.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": self.reply }
            }))
        }
    }

    fn cfg(url: &str) -> TurnDriverConfig {
        let mut c = TurnDriverConfig::new(url, "test-model", BackendKind::Ollama, ".");
        c.caveats = Caveats::top();
        c
    }

    #[test]
    fn constructors_capture_the_current_runtime_posture() {
        use crate::cognition::{set_cli_cognition, CognitionOverride};
        use crate::role_profile::Cognition;
        use crate::tenacity::{set_cli_tenacity, Tenacity};
        use crate::test_guard::GlobalSettingsGuard;

        let _settings = GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Set(Cognition::Pondering));
        set_cli_tenacity(Tenacity::Insistent);
        let empty = TurnDriver::new(cfg("http://placeholder"));

        set_cli_cognition(CognitionOverride::Off);
        set_cli_tenacity(Tenacity::Relaxed);
        let seeded = TurnDriver::with_transcript(
            cfg("http://placeholder"),
            vec![MemMessage::user("already started")],
        );

        assert_eq!(empty.runtime.cognition, Some(Cognition::Pondering));
        assert_eq!(empty.runtime.tenacity, Tenacity::Insistent);
        assert_eq!(seeded.runtime.cognition, None);
        assert_eq!(seeded.runtime.tenacity, Tenacity::Relaxed);
    }

    /// Pump the driver to completion the way a crossterm loop would: poll on an
    /// interval until the turn resolves, never blocking the "frame".
    ///
    /// The budget is a WALL-CLOCK deadline, not a poll count. It used to be 600
    /// polls at 10 ms, which reads like six seconds but is really "600 polls
    /// plus however long 600 `poll()` calls take" — and under `cargo llvm-cov`
    /// instrumentation the turns that do a real mock-server round trip, take a
    /// 400, compact, and retry ran past it. That made the test fail by machine
    /// speed rather than by behaviour.
    ///
    /// A deadline says what is meant: no turn here should take 30 s, and one
    /// that does is hung, which is what this guard exists to catch.
    async fn pump_to_done(driver: &mut TurnDriver) -> TurnStatus {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            match driver.poll() {
                TurnStatus::Running => tokio::time::sleep(Duration::from_millis(10)).await,
                other => return other,
            }
        }
        panic!("turn did not complete within the 30s pump budget");
    }

    /// THE acceptance test: a consumer drives one turn through the driver and
    /// gets the answer back — no `run_chat`, no blocking REPL, only the
    /// submit → poll surface.
    #[tokio::test]
    async fn driver_pumps_one_turn_and_yields_the_answer() {
        let server = MockServer::start().await;
        let served = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(PlainOllama {
                served: served.clone(),
                reply: "the driver answered".into(),
            })
            .mount(&server)
            .await;

        let mut driver = TurnDriver::new(cfg(&server.uri()));
        // Idle before anything is submitted.
        assert!(matches!(driver.poll(), TurnStatus::Idle));
        assert!(!driver.is_running());

        driver
            .submit("what is 2 + 2?")
            .expect("submit starts a turn");
        assert!(driver.is_running());

        let status = pump_to_done(&mut driver).await;
        match status {
            TurnStatus::Completed(outcome) => {
                assert_eq!(outcome.reply, "the driver answered");
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        // The transcript now holds the user turn AND the assistant reply, with
        // no run_chat involved.
        let t = driver.transcript();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].role, Role::User);
        assert_eq!(t[0].content, "what is 2 + 2?");
        assert_eq!(t[1].role, Role::Assistant);
        assert_eq!(t[1].content, "the driver answered");

        // The backend served exactly the streaming re-issue + probe (the
        // tools-present probe then the stream); at least one request landed.
        assert!(served.load(Ordering::SeqCst) >= 1);
        // Drained — back to idle.
        assert!(matches!(driver.poll(), TurnStatus::Idle));
        assert!(!driver.is_running());
    }

    /// #1280: a stub embedder so the driver can carry a `code_search` handle.
    /// Advertisement is gated on `code_search.is_some()`, so `embed` need not be
    /// called for this test — a fixed vector keeps it total if it ever is.
    struct StubEmbedder;
    #[async_trait::async_trait]
    impl Embedder for StubEmbedder {
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.0_f32; 4])
        }
    }

    /// Ollama responder that records every chat request body, so a test can
    /// assert what tools the driver advertised.
    struct CapturingOllama {
        bodies: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        reply: String,
    }
    impl Respond for CapturingOllama {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&req.body) {
                self.bodies.lock().unwrap().push(v);
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": self.reply }
            }))
        }
    }

    /// True if any recorded request advertised a tool named `code_search`.
    fn advertised_code_search(bodies: &[serde_json::Value]) -> bool {
        bodies.iter().any(|b| {
            b["tools"]
                .as_array()
                .is_some_and(|ts| ts.iter().any(|t| t["function"]["name"] == "code_search"))
        })
    }

    fn advertised_tool(bodies: &[serde_json::Value], name: &str) -> bool {
        bodies.iter().any(|body| {
            body["tools"].as_array().is_some_and(|tools| {
                tools
                    .iter()
                    .any(|tool| tool["function"]["name"].as_str() == Some(name))
            })
        })
    }

    struct StubCrewRunner;

    #[async_trait::async_trait]
    impl CrewRunner for StubCrewRunner {
        async fn dispatch(
            &self,
            _op: &str,
            _args: &serde_json::Value,
            _caveats: &Caveats,
        ) -> Result<String, String> {
            Ok("unused fixture crew".to_string())
        }
    }

    async fn drive_once_capturing(mut driver: TurnDriver) -> Vec<serde_json::Value> {
        let server = MockServer::start().await;
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(CapturingOllama {
                bodies: bodies.clone(),
                reply: "done".into(),
            })
            .mount(&server)
            .await;
        // Rebind the driver's URL to this server (the caller built it with a
        // placeholder so the code_search wiring is the only variable).
        driver.config.url = server.uri();
        driver.submit("find the retry backoff").expect("submit");
        let _ = pump_to_done(&mut driver).await;
        let out = bodies.lock().unwrap().clone();
        out
    }

    /// #1280: the headless driver advertises `code_search` **iff** the config
    /// carries a retrieval handle — the fix for the hardcoded `code_search: None`
    /// that left crew leaves with no semantic retrieval.
    #[tokio::test]
    async fn code_search_advertised_only_when_the_config_carries_retrieval() {
        // Without retrieval: the tool is absent (unchanged headless behavior).
        let bare = drive_once_capturing(TurnDriver::new(cfg("http://placeholder"))).await;
        assert!(
            !advertised_code_search(&bare),
            "no code_search tool without a configured index"
        );

        // With a supplied embedder + index: the tool is advertised + executable.
        let index: Arc<dyn SemanticIndex> = Arc::new(SessionSemanticIndex::default());
        let with = drive_once_capturing(TurnDriver::new(
            cfg("http://placeholder").with_code_search(Arc::new(StubEmbedder), index, 3),
        ))
        .await;
        assert!(
            advertised_code_search(&with),
            "code_search advertised once the driver carries retrieval"
        );
    }

    /// Grounds the owned thread-crossing seam itself: a configured runner is
    /// what advertises both crew tools, and absence keeps both off the wire.
    #[tokio::test]
    async fn crew_advertised_only_when_driver_carries_a_runner() {
        let bare = drive_once_capturing(TurnDriver::new(cfg("http://placeholder"))).await;
        assert!(!advertised_tool(&bare, "crew"));
        assert!(!advertised_tool(&bare, "compose_roster"));

        let with = drive_once_capturing(
            TurnDriver::new(cfg("http://placeholder")).with_crew_runner(Arc::new(StubCrewRunner)),
        )
        .await;
        assert!(advertised_tool(&with, "crew"));
        assert!(advertised_tool(&with, "compose_roster"));
    }

    struct CrewCallingOllama;

    impl Respond for CrewCallingOllama {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("chat request JSON");
            let has_result = body["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
            if has_result {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "crew result reviewed" }
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "tool_calls": [{
                            "function": {
                                "name": "crew",
                                "arguments": {
                                    "task": "repair qualification harness",
                                    "mode": "crew",
                                    "crew": "nemotron-pair",
                                    "verify": "just check"
                                }
                            }
                        }]
                    }
                }))
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct CrewDispatch {
        op: String,
        args: serde_json::Value,
        caveats: Caveats,
        tenacity: crate::tenacity::Tenacity,
    }

    struct RecordingCrewRunner {
        dispatches: Arc<std::sync::Mutex<Vec<CrewDispatch>>>,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for RecordingCrewRunner {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl CrewRunner for RecordingCrewRunner {
        async fn dispatch(
            &self,
            op: &str,
            args: &serde_json::Value,
            caveats: &Caveats,
        ) -> Result<String, String> {
            self.dispatches.lock().unwrap().push(CrewDispatch {
                op: op.to_string(),
                args: args.clone(),
                caveats: caveats.clone(),
                tenacity: crate::tenacity::effective_tenacity(),
            });
            Ok("crew ran: diff +3/-1; just check PASS".to_string())
        }
    }

    /// Grounds the full model → agent loop → crew seam with a real HTTP turn.
    /// The owned `Arc` must cross the dedicated driver thread without losing
    /// the model's arguments or attenuated session authority.
    #[tokio::test]
    async fn model_crew_call_dispatches_exact_args_caveats_and_captured_posture() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(CrewCallingOllama)
            .mount(&server)
            .await;

        let expected_caveats = Caveats {
            fs_read: Scope::only(["/qualification/read".to_string()]),
            fs_write: Scope::only(["/qualification/write".to_string()]),
            exec: Scope::only(["just".to_string()]),
            net: Scope::only(["127.0.0.1".to_string()]),
            max_calls: CountBound::AtMost(9),
            valid_for_generation: Scope::only([42]),
        };
        let expected_args = serde_json::json!({
            "task": "repair qualification harness",
            "mode": "crew",
            "crew": "nemotron-pair",
            "verify": "just check"
        });
        let dispatches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let drops = Arc::new(AtomicUsize::new(0));
        let runner = Arc::new(RecordingCrewRunner {
            dispatches: dispatches.clone(),
            drops: drops.clone(),
        });
        let weak_runner = Arc::downgrade(&runner);

        let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
        crate::tenacity::set_cli_tenacity(crate::tenacity::Tenacity::Standard);

        let mut config = cfg(&server.uri());
        config.caveats = expected_caveats.clone();
        let mut driver = TurnDriver::new(config)
            .with_cognition(Some(crate::role_profile::Cognition::Pondering))
            .with_tenacity(crate::tenacity::Tenacity::Relentless)
            .with_crew_runner(runner.clone());
        crate::tenacity::set_cli_tenacity(crate::tenacity::Tenacity::Relaxed);
        drop(runner);
        assert!(
            weak_runner.upgrade().is_some(),
            "the driver owns the only remaining crew runner Arc"
        );

        driver.submit("field the approved crew").expect("submit");
        let TurnStatus::Completed(outcome) = pump_to_done(&mut driver).await else {
            panic!("crew turn did not complete")
        };
        assert_eq!(outcome.error, None);
        assert_eq!(outcome.reply, "crew result reviewed");
        assert_eq!(
            dispatches.lock().unwrap().as_slice(),
            &[CrewDispatch {
                op: "crew".to_string(),
                args: expected_args,
                caveats: expected_caveats,
                tenacity: crate::tenacity::Tenacity::Relentless,
            }]
        );

        drop(driver);
        assert!(weak_runner.upgrade().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    /// Integrated regression for headless numbered recovery: the declared 65K
    /// window is stale, vLLM reports its actual 32K full window, and
    /// contemplating reserves 16K output. The parser must therefore compact
    /// toward 16,768 input tokens; the old numberless fallback (80% of the
    /// current cap twice) leaves this
    /// fixture above the server threshold and cannot succeed.
    #[tokio::test]
    async fn headless_vllm_400_recovers_with_full_window_output_reserve() {
        const SERVER_MESSAGE_CHAR_LIMIT: usize = 16_768 * 4;
        struct VllmWindow {
            calls: Arc<AtomicUsize>,
            message_chars: Arc<std::sync::Mutex<Vec<usize>>>,
        }
        impl Respond for VllmWindow {
            fn respond(&self, request: &Request) -> ResponseTemplate {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let body: serde_json::Value =
                    serde_json::from_slice(&request.body).expect("chat request JSON");
                assert_eq!(body["max_tokens"], 16_000);
                let chars = body["messages"]
                    .as_array()
                    .expect("messages array")
                    .iter()
                    .filter_map(|message| message["content"].as_str())
                    .map(str::len)
                    .sum();
                self.message_chars.lock().unwrap().push(chars);
                if chars > SERVER_MESSAGE_CHAR_LIMIT {
                    ResponseTemplate::new(400).set_body_string(
                        "This model's maximum context length is 32768 tokens. However, you requested 16000 output tokens and your prompt contains 20000 input tokens, for a total of 36000 tokens (20000 + 16000 = 36000 > 32768). Please reduce the length of the input prompt or the number of requested output tokens.",
                    )
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "model": "test-model",
                        "choices": [{
                            "message": {"role": "assistant", "content": "recovered"},
                            "finish_reason": "stop"
                        }],
                        "usage": {
                            "prompt_tokens": 16_000,
                            "completion_tokens": 1,
                            "total_tokens": 16_001
                        }
                    }))
                }
            }
        }

        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let message_chars = Arc::new(std::sync::Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(VllmWindow {
                calls: calls.clone(),
                message_chars: message_chars.clone(),
            })
            .mount(&server)
            .await;

        let mut config =
            TurnDriverConfig::new(server.uri(), "test-model", BackendKind::Openai, ".");
        config.chat_completions_capability = crate::model_card::ChatCompletionsCapability {
            cognition: Some(true),
            ..Default::default()
        };
        config.num_ctx = Some(65_536);
        config.safe_context = Some(52_428);
        config.max_ok_input = Some(52_428);

        let mut transcript = Vec::new();
        for index in 0..20 {
            let content = format!("history-{index:02} {}", "x".repeat(3_988));
            transcript.push(if index % 2 == 0 {
                MemMessage::user(content)
            } else {
                MemMessage::assistant(content)
            });
        }
        let estimated_history = config
            .estimation
            .tokens_for_chars(transcript.iter().map(|m| m.content.len()).sum());
        assert!(
            (16_769..31_702).contains(&estimated_history),
            "fixture must distinguish full-window recovery from two 80% fallbacks: {estimated_history}"
        );

        let mut driver = TurnDriver::with_transcript(config, transcript)
            .with_cognition(Some(crate::role_profile::Cognition::Contemplating));
        driver.submit("finish the current task").expect("submit");
        let TurnStatus::Completed(outcome) = pump_to_done(&mut driver).await else {
            panic!("headless turn did not complete after numbered recovery")
        };
        assert_eq!(outcome.error, None, "numbered recovery must be clean");
        assert_eq!(outcome.reply, "recovered");
        assert!(calls.load(Ordering::SeqCst) >= 2);
        let observed = message_chars.lock().unwrap();
        assert!(
            observed[0] > SERVER_MESSAGE_CHAR_LIMIT,
            "first request must overflow"
        );
        assert!(
            *observed.last().unwrap() <= SERVER_MESSAGE_CHAR_LIMIT,
            "retry must compact under the 16,768-token input cap: {observed:?}"
        );
    }

    /// A shell observation feeds the NEXT turn's context, redacted. We prove the
    /// observation reaches the backend's request body (so the model sees it) and
    /// that a secret in it never does.
    #[tokio::test]
    async fn observation_is_in_context_for_the_next_turn_and_redacted() {
        let server = MockServer::start().await;
        // Capture the request body the model would see.
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        struct Capture {
            seen: Arc<std::sync::Mutex<String>>,
        }
        impl Respond for Capture {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                *self.seen.lock().unwrap() = String::from_utf8_lossy(&req.body).into_owned();
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "ack" }
                }))
            }
        }
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(Capture { seen: seen.clone() })
            .mount(&server)
            .await;

        let mut driver = TurnDriver::new(cfg(&server.uri()));
        driver.submit_observation(ShellObservation::new(
            "zsh",
            "$ cat creds\nsecret_key=wJalrXUtnFEMIabcdEFGHIJKLMNOPbPxRfiCY",
        ));
        // Observation alone does not start a turn.
        assert!(!driver.is_running());
        assert_eq!(driver.transcript().len(), 1);

        driver.submit("did you see my shell?").expect("submit");
        let _ = pump_to_done(&mut driver).await;

        let body = seen.lock().unwrap().clone();
        // The observation framing reached the model …
        assert!(
            body.contains("shell observation"),
            "observation missing from request body: {body}"
        );
        // … but the secret value did not.
        assert!(
            !body.contains("wJalrXUtnFEMIabcdEFGHIJKLMNOPbPxRfiCY"),
            "secret leaked into the request body: {body}"
        );
    }

    /// Submitting while a turn is in flight is rejected — one turn at a time.
    #[tokio::test]
    async fn second_submit_while_running_is_busy() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            // Delay so the first turn is reliably still in flight.
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(200))
                    .set_body_json(serde_json::json!({ "message": { "content": "slow" } })),
            )
            .mount(&server)
            .await;

        let mut driver = TurnDriver::new(cfg(&server.uri()));
        driver.submit("first").expect("first submit");
        let err = driver
            .submit("second")
            .expect_err("second must be rejected");
        assert!(matches!(err, TurnDriverError::Busy));
        // Let it finish so the test doesn't leak the task.
        let _ = pump_to_done(&mut driver).await;
    }

    /// W0 (#1511): the served model + parse signals flow from the loop to the
    /// `TurnOutcome`. The backend body carries `model` (what it actually
    /// served); the reply is content with no tool call, so round 0 records a
    /// `no_parseable_tool_call` signal. Clean turn ⇒ no error class.
    #[tokio::test]
    async fn outcome_carries_served_model_and_parse_signals() {
        use crate::agentic::observability::ParseSignal;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "served-by-backend",
                "message": { "content": "plain prose, no tool call" }
            })))
            .mount(&server)
            .await;

        let mut driver = TurnDriver::new(cfg(&server.uri()));
        driver.submit("do a thing").expect("submit");
        let status = pump_to_done(&mut driver).await;
        let TurnStatus::Completed(o) = status else {
            panic!("expected Completed, got {status:?}");
        };
        assert_eq!(o.error, None);
        assert_eq!(o.error_class, None, "clean turn carries no error class");
        assert_eq!(
            o.served_model.as_deref(),
            Some("served-by-backend"),
            "effective model comes from the response body, not our request"
        );
        assert!(
            o.parse_signals
                .contains(&ParseSignal::NoParseableToolCall { round: 0 }),
            "content with no parseable call is signalled: {:?}",
            o.parse_signals
        );
    }

    /// W0 (#1511): a non-success HTTP status is a `model_error` STRUCTURALLY —
    /// the class rides the typed `DispatchError` through the anyhow chain to
    /// the outcome; nothing string-matches the message. (404 is non-retryable,
    /// so the test does not sit through the backoff schedule.)
    #[tokio::test]
    async fn http_error_status_classifies_as_model_error() {
        use crate::agentic::observability::ErrorClass;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(404).set_body_string("no such model"))
            .mount(&server)
            .await;

        let mut driver = TurnDriver::new(cfg(&server.uri()));
        driver.submit("do a thing").expect("submit");
        let status = pump_to_done(&mut driver).await;
        let TurnStatus::Completed(o) = status else {
            panic!("expected Completed-with-error, got {status:?}");
        };
        let err = o
            .error
            .expect("the failed dispatch is carried on the outcome");
        assert!(err.contains("Ollama 404"), "historical wording kept: {err}");
        assert_eq!(o.error_class, Some(ErrorClass::Model));
    }

    /// Cancel aborts the in-flight turn and returns the driver to idle.
    #[tokio::test]
    async fn cancel_aborts_the_in_flight_turn() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(30))
                    .set_body_json(serde_json::json!({ "message": { "content": "never" } })),
            )
            .mount(&server)
            .await;

        let mut driver = TurnDriver::new(cfg(&server.uri()));
        driver.submit("start a slow turn").expect("submit");
        assert!(driver.is_running());
        driver.cancel();
        assert!(!driver.is_running());
        assert!(matches!(driver.poll(), TurnStatus::Idle));
        // The user message that started it survives for a clean retry.
        assert_eq!(driver.transcript().len(), 1);
        assert_eq!(driver.transcript()[0].content, "start a slow turn");
    }
}
