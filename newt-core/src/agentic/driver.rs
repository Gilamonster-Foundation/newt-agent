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

use super::observation::ShellObservation;
use super::{chat_complete, openai_chat_complete, ChatCtx, NoMcp};
use crate::{BackendKind, MemMessage, Role, TokenUsage};

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
    /// Absolute workspace path the tool loop runs against.
    pub workspace: String,
    /// Permission caveats enforced for this turn's tool calls.
    pub caveats: crate::caveats::Caveats,
    /// Maximum tool-call rounds before a forced final completion.
    pub max_tool_rounds: usize,
    /// Additional progress-aware rounds after `max_tool_rounds`; `0` makes the
    /// normal cap hard.
    pub workflow_grace_rounds: usize,
    /// Max lines of tool output shown inline.
    pub tool_output_lines: usize,
    /// Ollama `options.num_ctx` (ignored on the OpenAI path).
    pub num_ctx: Option<u32>,
    /// TCP connect timeout.
    pub connect_timeout_secs: u64,
    /// Total inference timeout.
    pub inference_timeout_secs: u64,
    /// Message count at which the in-flight conversation is trimmed mid-turn.
    pub mid_loop_trim_threshold: usize,
    /// Token threshold that also triggers a mid-loop trim. `None` disables it.
    pub mid_loop_trim_tokens: Option<usize>,
    /// Highest input-token count the model has accepted (pre-send budget gate).
    pub max_ok_input: Option<u32>,
    /// Shell command run after every successful write for ground-truth feedback.
    pub build_check_cmd: Option<String>,
    /// Empirically safe context size used to detect likely overflow.
    pub safe_context: Option<u32>,
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
            workspace: workspace.into(),
            caveats: crate::caveats::Caveats::top(),
            max_tool_rounds: 40,
            workflow_grace_rounds: 5,
            tool_output_lines: 20,
            num_ctx: None,
            connect_timeout_secs: 5,
            inference_timeout_secs: 120,
            mid_loop_trim_threshold: 40,
            mid_loop_trim_tokens: None,
            max_ok_input: None,
            build_check_cmd: None,
            safe_context: None,
        }
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

/// A pumpable, non-blocking driver for one agentic turn at a time.
///
/// Owns the running transcript and at most one in-flight turn. The external
/// event loop [`submit`](Self::submit)s input (or
/// [`submit_observation`](Self::submit_observation)s shell activity),
/// [`poll`](Self::poll)s for completion every frame, and may
/// [`cancel`](Self::cancel) the in-flight turn.
pub struct TurnDriver {
    config: TurnDriverConfig,
    transcript: Vec<MemMessage>,
    in_flight: Option<InFlight>,
}

impl TurnDriver {
    /// Build a driver with an empty transcript.
    pub fn new(config: TurnDriverConfig) -> Self {
        Self {
            config,
            transcript: Vec::new(),
            in_flight: None,
        }
    }

    /// Build a driver seeded with an existing transcript (e.g. a system prompt
    /// plus prior turns the consumer already assembled).
    pub fn with_transcript(config: TurnDriverConfig, transcript: Vec<MemMessage>) -> Self {
        Self {
            config,
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
                    out = run_one_turn(&config, &messages, &task) => out,
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
    messages: &[MemMessage],
    task: &str,
) -> Result<TurnOutcome, String> {
    let ctx = ChatCtx {
        url: &config.url,
        model: &config.model,
        kind: config.kind,
        api_key: config.api_key.as_deref(),
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
        compaction_store: None,
        scratchpad: false,
        scratchpad_store: None,
        code_search: None,
        experience_store: None,
        step_ledger: None,
        caveats: &config.caveats,
        max_tool_rounds: config.max_tool_rounds,
        workflow_grace_rounds: config.workflow_grace_rounds,
        tool_output_lines: config.tool_output_lines,
        debug: false,
        trace: false,
        num_ctx: config.num_ctx,
        connect_timeout_secs: config.connect_timeout_secs,
        inference_timeout_secs: config.inference_timeout_secs,
        mid_loop_trim_threshold: config.mid_loop_trim_threshold,
        mid_loop_trim_tokens: config.mid_loop_trim_tokens,
        max_ok_input: config.max_ok_input,
        build_check_cmd: config.build_check_cmd.clone(),
        safe_context: config.safe_context,
        // Session-bound seams a driven cowork turn does not carry (headless,
        // exactly like the ACP worker / newt-eval callers).
        recover_cw_400: None,
        note_sink: None,
        note_nudge: None,
        recall_source: None,
        memory_source: None,
        summarizer: None,
        compress_state: None,
        tool_events: None,
        phantom_reaches: None,
        permission_gate: None,
        // Phase 20 (spec §5): headless surfaces neither read nor write the
        // capability cache — the hook stays absent and no calibration is
        // applied, preserving today's behavior exactly.
        on_round_usage: None,
        estimate_ratio: None,
        estimation: crate::tokens::TokenEstimation::default(),
        summary_input_cap_floor_chars: 8_192,
        // #307: the headless driver carries no preset clamp — exec authority is
        // whatever `config.caveats` already grants, exactly like the ACP worker
        // / newt-eval callers. A consumer enforcing a mode clamps `caveats` itself.
        exec_floor: None,
        write_ledger: None,
        // The headless driver has no interactive keyboard to interrupt from;
        // a consumer that wants cancellation drives its own ChatCtx.
        cancel: None,
        git_tool: None,
        crew_runner: None,
    };
    // NoMcp: the cowork driver advertises only the built-in tools. A consumer
    // that wants live MCP tools assembles its own ChatCtx.
    let mut mcp = NoMcp;
    let dispatch = if config.kind == BackendKind::Openai {
        openai_chat_complete(ctx, &mut mcp).await
    } else {
        chat_complete(ctx, &mut mcp).await
    };
    match dispatch {
        Ok((reply, was_streamed, usage, hallucinations)) => Ok(TurnOutcome {
            reply,
            was_streamed,
            usage,
            hallucinations,
        }),
        Err(e) => Err(e.to_string()),
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
    use crate::caveats::Caveats;
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

    /// Pump the driver to completion the way a crossterm loop would: poll on an
    /// interval until the turn resolves, never blocking the "frame".
    async fn pump_to_done(driver: &mut TurnDriver) -> TurnStatus {
        for _ in 0..600 {
            match driver.poll() {
                TurnStatus::Running => tokio::time::sleep(Duration::from_millis(10)).await,
                other => return other,
            }
        }
        panic!("turn did not complete within the pump budget");
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
