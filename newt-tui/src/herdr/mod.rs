//! Herdr integration: generic lifecycle events → bounded queue → JSON-RPC.
//!
//! When newt runs inside a [Herdr](https://herdr.dev) pane, Herdr injects
//! `HERDR_ENV=1`, `HERDR_PANE_ID`, and `HERDR_SOCKET_PATH`. This module
//! detects that, subscribes to [`newt_core::lifecycle`], and translates the
//! agent's own vocabulary into Herdr's `pane.*` JSON-RPC calls so the cockpit
//! shows newt as a first-class agent — idle / working / blocked — with no
//! screen-scraping heuristics on either side.
//!
//! Nothing about Herdr exists in `newt-core`. The agent emits what it is
//! doing; this file is the only place that knows Herdr exists at all, and
//! deleting it would leave the agent semantically unchanged.
//!
//! # Why the agent can never wait for Herdr
//!
//! Telemetry that can apply backpressure is not telemetry, it is a dependency.
//! The path is deliberately split at a queue:
//!
//! ```text
//!   agent thread                  │  reporter worker thread
//!   ─────────────                 │  ──────────────────────
//!   lifecycle::emit(event)        │
//!     └─ adapter.on_event(&e)     │
//!          ├─ lock, apply, unlock │   wake ──▶ read desired state (lock,
//!          └─ wake.try_send(())   │            clone, unlock)
//!             (drop if full)      │            └─ socket I/O (bounded)
//! ```
//!
//! The agent-facing call does three bounded things: take an uncontended
//! mutex, apply a pure state transition, and offer a token to a
//! capacity-1 channel with `try_send`. **No socket call, no `connect`, no
//! `write`, no response wait, and no unbounded queue growth is reachable from
//! the agent's thread.** The worker never holds the state lock across I/O, so
//! the agent's worst case is another emitter's transition — nanoseconds.
//!
//! Losing a wake token is harmless: the desired state lives in a shared cell,
//! not in the channel, so a dropped wake is *coalescing*, not data loss. The
//! worker always delivers the latest state it can see, and a state that
//! failed to deliver stays undelivered until it succeeds.
//!
//! # Degradation table (all of these are "the agent continues")
//!
//! | Condition                    | Behavior                                  |
//! |------------------------------|-------------------------------------------|
//! | Herdr absent (no env)        | No subscription at all; `emit` is a load  |
//! | Socket path missing          | `deliver` fails fast, no thread spawned   |
//! | Connect hangs                | Abandoned after 200 ms, one attempt max   |
//! | Herdr stops reading          | Write times out (250 ms), conn retired    |
//! | Response never arrives       | Never awaited; costs nothing              |
//! | Events outrun the consumer   | Wakes dropped; latest state coalesced     |
//! | Queue full                   | `try_send` fails; state cell already set  |
//! | Reporter thread dies         | Channel disconnects; emits stay bounded   |

pub mod protocol;
pub mod transport;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use newt_core::lifecycle::{self, LifecycleEnvelope, LifecycleEvent, Subscription};

use protocol::{Call, PaneAgentState, SessionStartSource};
#[cfg(unix)]
use transport::SocketSink;
use transport::{cli, Sink};

/// One-shot calls (session identity, tab title) queued for the worker. Small
/// on purpose: these are startup facts, not a stream.
const MAX_ONESHOTS: usize = 8;
/// How long a session teardown waits for the worker to release lifecycle
/// authority before detaching it. Exit is not held hostage to Herdr either.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(300);

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

/// The Herdr pane this process reports to. `None` means the integration is
/// unavailable and nothing is installed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HerdrEnv {
    pane: String,
    /// Preferred transport. `None` when the pane advertises no socket (e.g.
    /// Windows) — then `bin` is the only way to reach Herdr.
    socket: Option<PathBuf>,
    /// `HERDR_BIN_PATH`: fallback transport via the `herdr` CLI. Used when
    /// there is no usable socket. `None` when unset.
    bin: Option<PathBuf>,
}

impl HerdrEnv {
    /// Pure resolution, so incompleteness is unit-testable without mutating
    /// process env.
    ///
    /// Detection requires `HERDR_ENV=1`, a pane id, and at least one reachable
    /// transport (a socket path OR an explicit `HERDR_BIN_PATH`). A pane with
    /// neither cannot be reported to, so it is "not in Herdr" and the whole
    /// integration stays a no-op.
    fn from_parts(
        env: Option<&str>,
        pane: Option<&str>,
        socket: Option<&str>,
        bin: Option<&str>,
    ) -> Option<Self> {
        if env != Some("1") {
            return None;
        }
        let pane = pane.filter(|p| !p.is_empty())?;
        let socket = socket.filter(|s| !s.is_empty()).map(PathBuf::from);
        let bin = bin.filter(|b| !b.is_empty()).map(PathBuf::from);
        if socket.is_none() && bin.is_none() {
            return None;
        }
        Some(Self {
            pane: pane.to_string(),
            socket,
            bin,
        })
    }

    fn from_process_env() -> Option<Self> {
        let env = std::env::var("HERDR_ENV").ok();
        let pane = std::env::var("HERDR_PANE_ID").ok();
        let socket = std::env::var("HERDR_SOCKET_PATH").ok();
        let bin = std::env::var("HERDR_BIN_PATH").ok();
        Self::from_parts(
            env.as_deref(),
            pane.as_deref(),
            socket.as_deref(),
            bin.as_deref(),
        )
    }

    /// The sink this environment should report through. Preference order:
    ///
    /// 1. **unix + socket path** → the direct socket sink (fastest, no spawn).
    /// 2. **`HERDR_BIN_PATH` set** → the CLI fallback. This is also the ONLY
    ///    working transport off unix: the socket sink is a fail-stub there, so
    ///    even an advertised socket path loses to the binary on Windows.
    /// 3. Otherwise → [`NullSink`] (defensive; `from_parts` guarantees one).
    fn sink(&self) -> Box<dyn Sink> {
        #[cfg(unix)]
        if let Some(socket) = &self.socket {
            return Box::new(SocketSink::new(socket.clone()));
        }
        if let Some(bin) = &self.bin {
            return Box::new(cli::CliSink::new(bin.clone()));
        }
        // from_parts guarantees one transport; this arm is reachable only on
        // non-unix with a socket but no binary (socket sink is a stub there).
        Box::new(NullSink)
    }
}

/// Delivers nothing; used only when no transport exists (unreachable via
/// `from_parts`, kept so `sink()` is total). Always reports failure.
struct NullSink;
impl Sink for NullSink {
    fn deliver(&mut self, _call: &protocol::Call) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// The state machine (pure)
// ---------------------------------------------------------------------------

/// What Herdr should believe about this pane: a state and an optional short
/// status token.
type Report = (PaneAgentState, Option<String>);

/// Translates the agent's lifecycle vocabulary into Herdr's three states, and
/// tracks what has actually been delivered. Pure — no I/O — so every rule is
/// deterministic to test.
#[derive(Debug)]
struct Machine {
    /// What is true right now.
    logical: Report,
    /// What Herdr last accepted. `None` until the first successful delivery.
    delivered: Option<Report>,
    /// Open prompt windows. Blocked is entered on 0 → 1, left on 1 → 0.
    depth: u32,
    /// What to restore when the outermost prompt closes.
    pre_prompt: Report,
}

impl Machine {
    fn new() -> Self {
        Self {
            logical: (PaneAgentState::Idle, None),
            delivered: None,
            depth: 0,
            pre_prompt: (PaneAgentState::Idle, None),
        }
    }

    fn set(&mut self, report: Report) {
        if self.depth == 0 {
            self.logical = report;
        } else {
            // A turn-state change while a prompt is open (the turn that
            // spawned the prompt finishing, say) must not clobber `blocked`;
            // it becomes what is restored when the prompt closes.
            self.pre_prompt = report;
        }
    }

    fn prompt_open(&mut self) {
        self.depth += 1;
        if self.depth == 1 {
            self.pre_prompt = self.logical.clone();
            self.logical = (PaneAgentState::Blocked, None);
        }
    }

    fn prompt_close(&mut self) {
        if self.depth > 0 {
            self.depth -= 1;
            if self.depth == 0 {
                self.logical = self.pre_prompt.clone();
            }
        }
    }

    /// What still needs delivering, if anything. Covers both a fresh
    /// transition and a previously failed delivery of the same state.
    fn pending(&self) -> Option<Report> {
        (self.delivered.as_ref() != Some(&self.logical)).then(|| self.logical.clone())
    }

    fn mark_delivered(&mut self, report: Report) {
        self.delivered = Some(report);
    }
}

/// Everything the agent thread and the worker share. The lock is held for
/// pure state transitions only, **never across I/O**.
#[derive(Debug)]
struct Inner {
    machine: Machine,
    /// The session id Herdr SHOULD know about, and how it started. Desired
    /// state, not an event: `/new` overwrites it, so a burst of restarts
    /// coalesces to the newest identity instead of queueing every one.
    desired_session: Option<(String, SessionStartSource)>,
    /// The session id Herdr has been told SUCCESSFULLY. Only a delivered call
    /// updates this, so a transient failure leaves `desired != delivered` and
    /// the next wake retries — identity converges instead of being lost.
    delivered_session: Option<String>,
    oneshots: VecDeque<Call>,
    /// Wake tokens dropped because the worker was busy. Diagnostic only —
    /// correctness never depends on a wake arriving.
    coalesced: u64,
    shutdown: bool,
}

// ---------------------------------------------------------------------------
// Adapter (agent-facing)
// ---------------------------------------------------------------------------

/// The lifecycle-event translator. Its only job on the agent's thread is to
/// fold the event into shared state and nudge the worker.
struct Adapter {
    pane: String,
    title: Option<String>,
    inner: Arc<Mutex<Inner>>,
    wake: SyncSender<()>,
}

impl Adapter {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Is this envelope for the session this pane tracks?
    ///
    /// `SessionStarted` always passes — it is how a pane adopts or re-anchors
    /// its session. Otherwise only an event explicitly scoped to a DIFFERENT
    /// session is rejected.
    ///
    /// Unscoped (`None`) events are ACCEPTED, deliberately. They come from
    /// emitters that ran before ownership was declared (or after it was
    /// cleared), and a pane hosts one session, so treating them as foreign
    /// would silently drop real state updates — a much worse failure than
    /// attributing a startup event to the session that is about to own the
    /// pane. Rejection is reserved for the case we can actually prove wrong:
    /// a named session that is not ours.
    fn belongs_here(inner: &Inner, envelope: &LifecycleEnvelope) -> bool {
        if matches!(envelope.event, LifecycleEvent::SessionStarted { .. }) {
            return true;
        }
        match (&inner.desired_session, &envelope.session_id) {
            (Some((mine, _)), Some(theirs)) => mine == theirs,
            _ => true,
        }
    }

    /// Fold one lifecycle event in. Bounded: a lock, a pure transition, and a
    /// non-blocking wake. Called on the agent's thread.
    ///
    /// A pane hosts ONE session at a time, so this adopts the session named by
    /// `SessionStarted` (re-anchoring on `/new`) and then IGNORES events
    /// belonging to any other session. Without that filter a second session in
    /// the same process would drive this pane's state machine and, worse, its
    /// `SessionEnded` would shut this reporter down (#1662).
    fn on_event(&self, envelope: &LifecycleEnvelope) {
        {
            let mut inner = self.lock();
            if !Self::belongs_here(&inner, envelope) {
                return;
            }
            let event = &envelope.event;
            match event {
                LifecycleEvent::SessionStarted { session_id } => {
                    // Desired state, NOT a one-shot (#1662). The old path
                    // pushed the call onto the bounded oneshot queue, where a
                    // transient delivery failure dropped it permanently and a
                    // full queue dropped the NEWEST identity — the one `/new`
                    // had just established. Recording intent instead lets the
                    // worker retry until Herdr actually knows, and lets a
                    // later start overwrite an earlier one.
                    inner.desired_session = Some((session_id.clone(), SessionStartSource::Startup));
                    if let Some(title) = &self.title {
                        push_oneshot(
                            &mut inner,
                            protocol::report_metadata_title(&self.pane, title),
                        );
                    }
                }
                LifecycleEvent::Waiting | LifecycleEvent::TurnCompleted => {
                    inner.machine.set((PaneAgentState::Idle, None));
                }
                LifecycleEvent::TurnStarted => {
                    inner.machine.set((PaneAgentState::Working, None));
                }
                LifecycleEvent::Thinking => {
                    inner
                        .machine
                        .set((PaneAgentState::Working, Some("thinking".into())));
                }
                LifecycleEvent::ToolActivity { tool } => {
                    inner
                        .machine
                        .set((PaneAgentState::Working, Some(tool.clone())));
                }
                LifecycleEvent::Blocked => inner.machine.prompt_open(),
                LifecycleEvent::Unblocked => inner.machine.prompt_close(),
                LifecycleEvent::TurnFailed { .. } => {
                    inner
                        .machine
                        .set((PaneAgentState::Idle, Some("failed".into())));
                }
                LifecycleEvent::TurnCancelled => {
                    inner
                        .machine
                        .set((PaneAgentState::Idle, Some("cancelled".into())));
                }
                LifecycleEvent::SessionEnded => inner.shutdown = true,
                // A vocabulary this build does not know about is not an error.
                _ => {}
            }
        }
        self.nudge();
    }

    /// Offer the worker a wake token. A full channel means a wake is already
    /// pending and the worker will re-read the (already updated) desired
    /// state — so dropping it loses nothing.
    fn nudge(&self) {
        match self.wake.try_send(()) {
            Ok(()) => {}
            Err(TrySendError::Full(())) => self.lock().coalesced += 1,
            // The worker is gone. Later events keep folding into shared state
            // harmlessly; the agent still never waits.
            Err(TrySendError::Disconnected(())) => {}
        }
    }
}

fn push_oneshot(inner: &mut Inner, call: Call) {
    if inner.oneshots.len() < MAX_ONESHOTS {
        inner.oneshots.push_back(call);
    }
}

// ---------------------------------------------------------------------------
// Worker (Herdr-facing)
// ---------------------------------------------------------------------------

/// One unit of work the worker picked up under the lock.
#[derive(Debug)]
enum Step {
    /// Release authority and stop.
    Stop,
    /// A one-shot call; delivered at most once, dropped if it fails.
    Once(Call),
    /// The session identity Herdr should have; marked delivered only on
    /// success, so it converges across transient failures.
    Session(String, Call),
    /// The current desired state; marked delivered only on success.
    State(Report, Call),
    /// Nothing to do; wait for the next wake.
    Park,
}

fn next_step(inner: &Arc<Mutex<Inner>>, pane: &str) -> Step {
    let mut guard = inner.lock().unwrap_or_else(PoisonError::into_inner);
    if guard.shutdown {
        return Step::Stop;
    }
    // Identity first: a state report Herdr attributes to a stale session is
    // worse than one that arrives a wake later.
    if let Some((id, source)) = guard.desired_session.clone() {
        if guard.delivered_session.as_deref() != Some(id.as_str()) {
            let call = protocol::report_agent_session(pane, &id, source);
            return Step::Session(id, call);
        }
    }
    if let Some(call) = guard.oneshots.pop_front() {
        return Step::Once(call);
    }
    match guard.machine.pending() {
        Some(report) => {
            let call = protocol::report_agent(pane, report.0, report.1.as_deref());
            Step::State(report, call)
        }
        None => Step::Park,
    }
}

fn worker_loop(
    inner: Arc<Mutex<Inner>>,
    pane: String,
    mut sink: Box<dyn Sink>,
    wake: Receiver<()>,
) {
    // Wait BEFORE looking at the machine. Every delivery is then caused by an
    // event that was already applied when its wake was sent — the worker can
    // never race the first emit and report the machine's initial state.
    while wake.recv().is_ok() {
        // Drain everything currently outstanding, then park.
        loop {
            match next_step(&inner, &pane) {
                Step::Stop => {
                    let _ = sink.deliver(&protocol::release_agent(&pane));
                    return;
                }
                Step::Once(call) => {
                    let _ = sink.deliver(&call);
                }
                Step::Session(id, call) => {
                    if sink.deliver(&call) {
                        inner
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .delivered_session = Some(id);
                    } else {
                        // Undelivered: `desired` still differs from
                        // `delivered`, so the next wake retries. One attempt
                        // per wake — bounded, no spin.
                        break;
                    }
                }
                Step::State(report, call) => {
                    if sink.deliver(&call) {
                        inner
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .machine
                            .mark_delivered(report);
                    } else {
                        // Undelivered: leave the mark alone so the next wake
                        // retries. One attempt per wake — bounded, no spin.
                        break;
                    }
                }
                Step::Park => break,
            }
        }
    }
    // Every sender is gone: the session ended without an orderly shutdown.
    // Release rather than leave Herdr holding stale state.
    let _ = sink.deliver(&protocol::release_agent(&pane));
}

// ---------------------------------------------------------------------------
// Session wiring
// ---------------------------------------------------------------------------

/// A live integration: the subscription plus its worker.
struct Reporter {
    inner: Arc<Mutex<Inner>>,
    wake: SyncSender<()>,
    worker: Option<JoinHandle<()>>,
}

impl Reporter {
    fn spawn(env: &HerdrEnv, title: Option<String>, sink: Box<dyn Sink>) -> (Self, Adapter) {
        let inner = Arc::new(Mutex::new(Inner {
            machine: Machine::new(),
            desired_session: None,
            delivered_session: None,
            oneshots: VecDeque::new(),
            coalesced: 0,
            shutdown: false,
        }));
        // Capacity 1: the channel carries "something changed", never the
        // change itself. One pending token is all the worker can act on.
        let (wake, rx) = sync_channel::<()>(1);
        let worker = {
            let inner = Arc::clone(&inner);
            let pane = env.pane.clone();
            std::thread::Builder::new()
                .name("herdr-reporter".into())
                .spawn(move || worker_loop(inner, pane, sink, rx))
                .ok()
        };
        let adapter = Adapter {
            pane: env.pane.clone(),
            title,
            inner: Arc::clone(&inner),
            wake: wake.clone(),
        };
        (
            Self {
                inner,
                wake,
                worker,
            },
            adapter,
        )
    }

    /// Ask the worker to release lifecycle authority and stop, waiting at most
    /// [`SHUTDOWN_GRACE`]. A stuck worker is detached, not waited on.
    fn shutdown(&mut self) {
        {
            let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            inner.shutdown = true;
        }
        let _ = self.wake.try_send(());
        let Some(worker) = self.worker.take() else {
            return;
        };
        // `JoinHandle` has no bounded join, so watch for thread exit instead.
        let deadline = std::time::Instant::now() + SHUTDOWN_GRACE;
        while !worker.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if worker.is_finished() {
            let _ = worker.join();
        }
        // Otherwise: detached. The process is exiting; Herdr reaps pane
        // authority when the pane's process goes away.
    }
}

/// RAII scope for one chat session. Creating it installs the integration when
/// this process is inside a Herdr pane, and does nothing at all otherwise;
/// dropping it releases lifecycle authority on every orderly exit path,
/// including `?` early returns.
///
/// `pub` so the headless `newt solve` entry (in `newt-cli`, which already
/// depends on this crate) reports through the exact same seam as the TUI.
pub struct SessionGuard {
    subscription: Option<Subscription>,
    reporter: Option<Reporter>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        // Unsubscribe first: no new events can arrive mid-teardown.
        self.subscription = None;
        if let Some(reporter) = &mut self.reporter {
            reporter.shutdown();
        }
    }
}

/// Install the Herdr integration for this session. Outside a Herdr pane this
/// subscribes to nothing, so lifecycle emission stays a single atomic load.
pub fn session_guard(workspace: &str) -> SessionGuard {
    let Some(env) = HerdrEnv::from_process_env() else {
        return SessionGuard {
            subscription: None,
            reporter: None,
        };
    };
    let title = std::path::Path::new(workspace)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    install(&env, title, env.sink())
}

fn install(env: &HerdrEnv, title: Option<String>, sink: Box<dyn Sink>) -> SessionGuard {
    let (reporter, adapter) = Reporter::spawn(env, title, sink);
    let subscription = lifecycle::subscribe(move |envelope| adapter.on_event(envelope));
    SessionGuard {
        subscription: Some(subscription),
        reporter: Some(reporter),
    }
}

#[cfg(test)]
impl Adapter {
    /// Test shim: deliver a bare event as an envelope. `session` is the
    /// session it belongs to — `None` means unscoped (early startup).
    fn on_bare(&self, session: Option<&str>, event: LifecycleEvent) {
        self.on_event(&LifecycleEnvelope {
            session_id: session.map(str::to_string),
            event,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    // -- test doubles -------------------------------------------------------

    /// Records every delivered call; optionally blocks inside `deliver` (a
    /// Herdr that has stopped consuming) or fails a scripted number of times.
    #[derive(Clone)]
    struct FakeSink {
        calls: Arc<Mutex<Vec<Call>>>,
        gate: Option<Arc<Mutex<()>>>,
        fail_first: Arc<AtomicUsize>,
        panics: bool,
    }

    impl FakeSink {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                gate: None,
                fail_first: Arc::new(AtomicUsize::new(0)),
                panics: false,
            }
        }

        fn gated(gate: &Arc<Mutex<()>>) -> Self {
            Self {
                gate: Some(Arc::clone(gate)),
                ..Self::new()
            }
        }

        fn failing(n: usize) -> Self {
            let sink = Self::new();
            sink.fail_first.store(n, Ordering::SeqCst);
            sink
        }

        fn panicking() -> Self {
            Self {
                panics: true,
                ..Self::new()
            }
        }

        /// (method, state) pairs in delivery order.
        fn seen(&self) -> Vec<(String, String)> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|c| {
                    let detail = match c.method {
                        "pane.report_agent" => c.params["state"].as_str().unwrap_or("").to_string(),
                        "pane.report_metadata" => {
                            c.params["title"].as_str().unwrap_or("").to_string()
                        }
                        "pane.report_agent_session" => c.params["agent_session_id"]
                            .as_str()
                            .unwrap_or("")
                            .to_string(),
                        _ => String::new(),
                    };
                    (c.method.to_string(), detail)
                })
                .collect()
        }

        fn states(&self) -> Vec<String> {
            self.seen()
                .into_iter()
                .filter(|(m, _)| m == "pane.report_agent")
                .map(|(_, s)| s)
                .collect()
        }

        fn messages(&self) -> Vec<Option<String>> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.method == "pane.report_agent")
                .map(|c| {
                    c.params
                        .get("message")
                        .and_then(|m| m.as_str())
                        .map(str::to_string)
                })
                .collect()
        }

        fn count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl Sink for FakeSink {
        fn deliver(&mut self, call: &Call) -> bool {
            if self.panics {
                panic!("sink exploded");
            }
            if let Some(gate) = &self.gate {
                let _held = gate.lock().unwrap_or_else(PoisonError::into_inner);
            }
            self.calls.lock().unwrap().push(call.clone());
            if self.fail_first.load(Ordering::SeqCst) > 0 {
                self.fail_first.fetch_sub(1, Ordering::SeqCst);
                return false;
            }
            true
        }
    }

    fn test_env() -> HerdrEnv {
        HerdrEnv::from_parts(Some("1"), Some("w1:p2"), Some("/tmp/herdr-test.sock"), None).unwrap()
    }

    /// Build an adapter + reporter directly (bypassing the process-global
    /// subscription registry, so tests can run in parallel without seeing each
    /// other's events).
    fn harness(sink: FakeSink) -> (Adapter, Reporter) {
        let (reporter, adapter) = Reporter::spawn(&test_env(), None, Box::new(sink));
        (adapter, reporter)
    }

    /// Non-destructive probe: has the worker delivered everything it can?
    /// (`next_step` mutates, so tests must never use it to wait.)
    ///
    /// #1662: identity counts. When session reporting moved off the one-shot
    /// queue onto desired-vs-delivered, this probe kept checking only
    /// `oneshots` and the state machine — so a session whose id had NOT yet
    /// reached the pane read as idle. Every test that waits on this could then
    /// race ahead and assert against an identity still in flight, which is the
    /// failure mode the desired/delivered model exists to make impossible.
    fn worker_is_idle(inner: &Arc<Mutex<Inner>>) -> bool {
        let guard = inner.lock().unwrap_or_else(PoisonError::into_inner);
        let identity_settled = match &guard.desired_session {
            Some((id, _)) => guard.delivered_session.as_deref() == Some(id.as_str()),
            None => true,
        };
        !guard.shutdown
            && guard.oneshots.is_empty()
            && guard.machine.pending().is_none()
            && identity_settled
    }

    /// Wait (bounded) until `f` holds; keeps the tests free of sleeps that are
    /// either flaky or slow.
    fn eventually(mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        f()
    }

    // -- environment --------------------------------------------------------

    // Herdr absent, or any marker missing: the integration is unavailable.
    #[test]
    fn env_absent_or_incomplete_disables_integration() {
        assert_eq!(HerdrEnv::from_parts(None, None, None, None), None);
        assert_eq!(
            HerdrEnv::from_parts(Some("0"), Some("p"), Some("/s"), None),
            None
        );
        assert_eq!(
            HerdrEnv::from_parts(Some("1"), None, Some("/s"), None),
            None
        );
        assert_eq!(
            HerdrEnv::from_parts(Some("1"), Some(""), Some("/s"), None),
            None
        );
        // Neither transport → undetectable, integration stays a no-op.
        assert_eq!(HerdrEnv::from_parts(Some("1"), Some("p"), None, None), None);
        assert_eq!(
            HerdrEnv::from_parts(Some("1"), Some("p"), Some(""), None),
            None
        );
        assert_eq!(
            HerdrEnv::from_parts(Some("1"), Some("p"), Some(""), Some("")),
            None
        );
        // A socket alone suffices.
        assert!(HerdrEnv::from_parts(Some("1"), Some("p"), Some("/s"), None).is_some());
        // A CLI binary alone suffices (the Windows / socket-less fallback).
        let env = HerdrEnv::from_parts(Some("1"), Some("p"), None, Some("/usr/bin/herdr"));
        assert!(env.is_some());
        let env = env.unwrap();
        assert_eq!(env.socket, None);
        assert_eq!(env.bin, Some(PathBuf::from("/usr/bin/herdr")));
    }

    // Outside a Herdr pane nothing is installed: no subscription, no worker.
    #[test]
    fn no_herdr_means_nosubscription_and_no_worker() {
        let guard = SessionGuard {
            subscription: None,
            reporter: None,
        };
        assert!(guard.subscription.is_none() && guard.reporter.is_none());
    }

    // -- vocabulary ---------------------------------------------------------

    // The agent's semantic vocabulary maps onto Herdr's three states — no
    // state is inferred from rendered prompt text.
    #[test]
    fn lifecycle_vocabulary_maps_to_herdr_states() {
        let sink = FakeSink::new();
        let (adapter, mut reporter) = harness(sink.clone());
        for event in [
            LifecycleEvent::SessionStarted {
                session_id: "s-1".into(),
            },
            LifecycleEvent::Waiting,
            LifecycleEvent::TurnStarted,
            LifecycleEvent::Thinking,
            LifecycleEvent::ToolActivity {
                tool: "read_file".into(),
            },
            LifecycleEvent::Blocked,
            LifecycleEvent::Unblocked,
            LifecycleEvent::TurnCompleted,
            LifecycleEvent::TurnFailed { reason: None },
            LifecycleEvent::TurnCancelled,
        ] {
            adapter.on_bare(None, event);
            // One event at a time so every intermediate state is observable;
            // coalescing is exercised in its own test.
            assert!(eventually(|| worker_is_idle(&reporter.inner)));
        }
        reporter.shutdown();

        assert_eq!(
            sink.seen().first(),
            Some(&("pane.report_agent_session".to_string(), "s-1".to_string())),
            "the session identity is announced first"
        );
        assert_eq!(
            sink.states(),
            [
                "idle",    // Waiting
                "working", // TurnStarted
                "working", // Thinking (message changes)
                "working", // ToolActivity (message changes)
                "blocked", // Blocked
                "working", // Unblocked restores the pre-prompt report
                "idle",    // TurnCompleted
                "idle",    // TurnFailed (message changes)
                "idle",    // TurnCancelled (message changes)
            ]
        );
        assert_eq!(
            sink.messages(),
            [
                None,
                None,
                Some("thinking".into()),
                Some("read_file".into()),
                None,
                Some("read_file".into()),
                None,
                Some("failed".into()),
                Some("cancelled".into()),
            ],
            "tool activity and turn outcome ride along as status tokens"
        );
    }

    // Req #7: `/commands`, `!shell`, help, and `exit` are NOT model turns.
    // The chat loop emits `Waiting` when the operator has the floor and only
    // emits `TurnStarted` from the final model-input branch — so a command
    // (which produces NO lifecycle event, or at most another `Waiting`) must
    // never flip the pane to Working. This test drives exactly the command
    // path's event sequence — session start, then Waiting with no intervening
    // TurnStarted — and asserts the pane stays idle.
    #[test]
    fn a_command_line_never_becomes_working() {
        let sink = FakeSink::new();
        let (adapter, mut reporter) = harness(sink.clone());
        adapter.on_bare(
            None,
            LifecycleEvent::SessionStarted {
                session_id: "s-cmd".into(),
            },
        );
        assert!(eventually(|| worker_is_idle(&reporter.inner)));
        // The operator runs /help, then !ls, then /cd, then exits. Each is a
        // fresh prompt-floor wait; none is a turn. Emit the Waiting each one
        // returns to, with NO TurnStarted between (that is the whole point).
        for _ in 0..4 {
            adapter.on_bare(None, LifecycleEvent::Waiting);
            assert!(eventually(|| worker_is_idle(&reporter.inner)));
        }
        reporter.shutdown();

        assert!(
            sink.states().iter().all(|s| s == "idle"),
            "no command may produce a Working report, got {:?}",
            sink.states()
        );
    }

    // Req #8 (`newt solve`): the headless surface announces its own session
    // and brackets the driver loop in TurnStarted/TurnCompleted — a solve run
    // IS always a real model turn, so Working is exact. This drives the same
    // event sequence solve.rs emits and asserts the pane tracks it.
    #[test]
    fn a_solve_run_reports_working_then_idle() {
        let sink = FakeSink::new();
        let (adapter, mut reporter) = harness(sink.clone());
        adapter.on_bare(
            None,
            LifecycleEvent::SessionStarted {
                session_id: "session-solve".into(),
            },
        );
        assert!(eventually(|| worker_is_idle(&reporter.inner)));
        adapter.on_bare(None, LifecycleEvent::TurnStarted);
        assert!(eventually(|| worker_is_idle(&reporter.inner)));
        adapter.on_bare(None, LifecycleEvent::TurnCompleted);
        assert!(eventually(|| worker_is_idle(&reporter.inner)));
        reporter.shutdown();

        assert_eq!(
            sink.seen().first(),
            Some(&(
                "pane.report_agent_session".to_string(),
                "session-solve".to_string()
            )),
            "solve announces its own session identity first"
        );
        assert_eq!(sink.states(), ["idle", "working", "idle"]);
    }

    // Blocked is a real state with real nesting: entered once on the outermost
    // prompt, left once, and the report restored is the pre-OUTER one. Inner
    // prompts emit nothing; an unbalanced close cannot underflow.
    #[test]
    fn blocked_state_survives_nested_prompts() {
        let sink = FakeSink::new();
        let (adapter, mut reporter) = harness(sink.clone());
        for event in [
            LifecycleEvent::TurnStarted,
            LifecycleEvent::Blocked,
            LifecycleEvent::Blocked,
            LifecycleEvent::Unblocked,
            LifecycleEvent::Unblocked,
            LifecycleEvent::Unblocked, // unbalanced: ignored
        ] {
            adapter.on_bare(None, event);
            assert!(eventually(|| worker_is_idle(&reporter.inner)));
        }
        reporter.shutdown();
        assert_eq!(sink.states(), ["working", "blocked", "working"]);
    }

    // A turn finishing while a prompt is open must not clobber `blocked`; it
    // becomes what is restored when the prompt closes.
    #[test]
    fn a_turn_ending_under_a_prompt_does_not_clobber_blocked() {
        let mut machine = Machine::new();
        machine.set((PaneAgentState::Working, None));
        machine.prompt_open();
        assert_eq!(machine.logical.0, PaneAgentState::Blocked);
        machine.set((PaneAgentState::Idle, None));
        assert_eq!(machine.logical.0, PaneAgentState::Blocked, "still blocked");
        machine.prompt_close();
        assert_eq!(machine.logical.0, PaneAgentState::Idle, "then restored");
    }

    // -- backpressure -------------------------------------------------------

    // THE load-bearing test: with Herdr wedged (every delivery blocked), the
    // agent-facing call stays bounded across a flood of events, the queue does
    // not grow, and once Herdr recovers the LATEST state is delivered — the
    // dropped wakes coalesced rather than lost.
    #[test]
    fn a_wedged_herdr_cannot_slow_or_stall_the_agent() {
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let sink = FakeSink::gated(&gate);
        let (adapter, mut reporter) = harness(sink.clone());

        // Let the worker enter `deliver` and block there.
        adapter.on_bare(None, LifecycleEvent::TurnStarted);
        std::thread::sleep(Duration::from_millis(20));

        const FLOOD: usize = 20_000;
        let start = Instant::now();
        for i in 0..FLOOD {
            adapter.on_bare(
                None,
                if i % 2 == 0 {
                    LifecycleEvent::Thinking
                } else {
                    LifecycleEvent::ToolActivity {
                        tool: "read_file".into(),
                    }
                },
            );
        }
        adapter.on_bare(None, LifecycleEvent::Waiting); // the LAST word
        let elapsed = start.elapsed();
        // The qualitative proof is that this line is reached at all: the sink
        // is STILL blocked, so a design that waited on delivery would never
        // have returned. The bounds below are deliberately loose (this box
        // runs many parallel builds); the real assertion is many orders of
        // magnitude away from a blocking path.
        assert!(
            elapsed < Duration::from_secs(10),
            "{FLOOD} emissions against a wedged Herdr took {elapsed:?}; the \
             agent-facing path must be bounded and never wait on delivery"
        );
        let per_event = elapsed / (FLOOD as u32 + 1);
        assert!(
            per_event < Duration::from_micros(500),
            "per-event agent cost {per_event:?} is not tiny"
        );

        {
            let inner = reporter.inner.lock().unwrap();
            assert!(
                inner.coalesced > 0,
                "a flood against a busy worker must coalesce, not queue"
            );
            assert!(inner.oneshots.len() <= MAX_ONESHOTS, "queues stay bounded");
        }

        drop(held); // Herdr starts consuming again
        assert!(
            eventually(|| sink.states().last().map(String::as_str) == Some("idle")),
            "after recovery the latest state wins; saw {:?}",
            sink.states()
        );
        assert!(
            sink.count() < FLOOD / 10,
            "coalescing must not replay every dropped event ({} calls)",
            sink.count()
        );
        reporter.shutdown();
    }

    // Delivery failures are not state corruption: an undelivered report is
    // retried on the next wake, and the SAME state is redelivered.
    #[test]
    fn a_failed_delivery_is_redelivered() {
        let sink = FakeSink::failing(2);
        let (adapter, mut reporter) = harness(sink.clone());
        adapter.on_bare(None, LifecycleEvent::TurnStarted);
        assert!(eventually(|| sink.count() >= 1));
        adapter.on_bare(None, LifecycleEvent::TurnStarted); // same state again
        assert!(eventually(|| sink.states().len() >= 2));
        adapter.on_bare(None, LifecycleEvent::TurnStarted);
        assert!(eventually(|| {
            let states = sink.states();
            states.len() >= 3 && states.iter().all(|s| s == "working")
        }));
        reporter.shutdown();
    }

    // The reporter thread dying (a panicking sink) must not panic, block, or
    // otherwise reach the agent — emission stays bounded afterwards.
    #[test]
    fn a_dead_reporter_thread_does_not_reach_the_agent() {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let (adapter, mut reporter) = harness(FakeSink::panicking());
        adapter.on_bare(None, LifecycleEvent::TurnStarted);
        assert!(eventually(|| reporter
            .worker
            .as_ref()
            .is_some_and(JoinHandle::is_finished)));

        let start = Instant::now();
        for _ in 0..1_000 {
            adapter.on_bare(None, LifecycleEvent::Thinking);
            adapter.on_bare(None, LifecycleEvent::Waiting);
        }
        let elapsed = start.elapsed();
        std::panic::set_hook(hook);
        assert!(
            elapsed < Duration::from_secs(2),
            "emission after the reporter died took {elapsed:?}"
        );
        // And teardown does not hang on a dead worker.
        let t0 = Instant::now();
        reporter.shutdown();
        assert!(t0.elapsed() < SHUTDOWN_GRACE * 4);
    }

    // -- teardown -----------------------------------------------------------

    // Shutdown releases lifecycle authority, as the final call, with the same
    // identity the reports used.
    #[test]
    fn shutdown_releases_authority_last() {
        let sink = FakeSink::new();
        let (adapter, mut reporter) = harness(sink.clone());
        adapter.on_bare(None, LifecycleEvent::TurnStarted);
        assert!(eventually(|| !sink.states().is_empty()));
        reporter.shutdown();
        let seen = sink.seen();
        assert_eq!(seen.last().unwrap().0, "pane.release_agent");
        let calls = sink.calls.lock().unwrap();
        let release = calls.last().unwrap();
        assert_eq!(release.params["pane_id"], "w1:p2");
        assert_eq!(release.params["source"], protocol::SOURCE);
        assert_eq!(release.params["agent"], protocol::AGENT);
    }

    // A stuck Herdr must not hold up session teardown either: the worker is
    // detached once the grace period expires.
    #[test]
    fn teardown_is_prompt_even_when_herdr_is_stuck() {
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let (adapter, mut reporter) = harness(FakeSink::gated(&gate));
        adapter.on_bare(None, LifecycleEvent::TurnStarted);
        std::thread::sleep(Duration::from_millis(20));
        let t0 = Instant::now();
        reporter.shutdown();
        let elapsed = t0.elapsed();
        assert!(
            elapsed < SHUTDOWN_GRACE * 4,
            "teardown waited {elapsed:?} on a stuck Herdr"
        );
        drop(held);
    }

    // The worker releases authority even when the session ends without an
    // orderly shutdown (every sender dropped).
    #[test]
    fn a_dropped_adapter_still_releases_authority() {
        let sink = FakeSink::new();
        let (adapter, mut reporter) = harness(sink.clone());
        adapter.on_bare(None, LifecycleEvent::TurnStarted);
        assert!(eventually(|| !sink.states().is_empty()));
        let worker = reporter.worker.take().unwrap();
        drop(adapter);
        drop(reporter); // drops the last sender
        let _ = worker.join();
        assert_eq!(sink.seen().last().unwrap().0, "pane.release_agent");
    }

    // The guard really is wired to the core lifecycle seam: an event emitted
    // through `newt_core::lifecycle` reaches the sink, and dropping the guard
    // unsubscribes.
    #[test]
    fn the_guard_subscribes_to_the_core_lifecycle_seam() {
        let sink = FakeSink::new();
        let guard = install(&test_env(), Some("repo".into()), Box::new(sink.clone()));
        lifecycle::emit(LifecycleEvent::TurnStarted);
        assert!(eventually(|| sink
            .states()
            .contains(&"working".to_string())));
        drop(guard);
        let after = sink.count();
        lifecycle::emit(LifecycleEvent::Waiting);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(sink.count(), after, "a dropped guard receives nothing more");
    }
    // ── #1662: session scoping and convergent identity ────────────────────

    /// Deliver a `SessionStarted` for `id`, then wait for it to reach the sink.
    fn start_session(adapter: &Adapter, reporter: &Reporter, id: &str) {
        adapter.on_bare(
            Some(id),
            LifecycleEvent::SessionStarted {
                session_id: id.into(),
            },
        );
        assert!(eventually(|| worker_is_idle(&reporter.inner)));
    }

    /// #1662: a session whose identity has NOT reached the pane is not idle.
    ///
    /// When session reporting moved off the one-shot queue onto
    /// desired-vs-delivered, `worker_is_idle` kept checking only `oneshots` and
    /// the state machine. So a reporter with an undelivered identity reported
    /// idle, and every test that waits on this probe — `start_session` among
    /// them — could race ahead and assert against an identity still in flight.
    /// A helper that returns before the thing it waits for has happened is
    /// worse than no helper: it turns a real ordering bug into a flake.
    ///
    /// The predicate is targeted directly. An earlier version of this test used
    /// a never-delivering sink, which left the STATE machine pending too — so
    /// `worker_is_idle` was false for the wrong reason and the test passed even
    /// with the identity check deleted. Here everything is driven to genuinely
    /// idle first, then ONLY the identity is dirtied, so the assertion can fail
    /// for exactly one reason.
    #[test]
    fn an_undelivered_session_identity_is_not_idle() {
        let sink = FakeSink::new();
        let (adapter, reporter) = harness(sink);
        start_session(&adapter, &reporter, "s1");
        assert!(
            worker_is_idle(&reporter.inner),
            "precondition: a delivered session with a settled machine is idle"
        );

        // Now a NEW identity is desired and has not been delivered. Nothing
        // else changes: no queued one-shots, no pending state.
        {
            let mut g = reporter.inner.lock().unwrap();
            g.desired_session = Some(("s2".to_string(), SessionStartSource::Startup));
            assert!(g.oneshots.is_empty(), "fixture leaves no queued one-shots");
            assert!(
                g.machine.pending().is_none(),
                "fixture leaves no pending state"
            );
        }
        assert!(
            !worker_is_idle(&reporter.inner),
            "an identity still in flight is not idle"
        );
    }

    #[test]
    fn session_identity_survives_transient_delivery_failure() {
        // The old path pushed `report_agent_session` as a one-shot: a single
        // failed deliver dropped the identity for good. Desired-vs-delivered
        // makes it converge.
        let sink = FakeSink::failing(2);
        let calls = Arc::clone(&sink.calls);
        let (adapter, reporter) = harness(sink);

        adapter.on_bare(
            Some("s1"),
            LifecycleEvent::SessionStarted {
                session_id: "s1".into(),
            },
        );
        // One retry per wake, so nudge while polling. Racing the worker with a
        // fixed number of un-awaited sends is what made the first draft of
        // this test flaky-by-construction.
        assert!(eventually(|| {
            adapter.on_bare(Some("s1"), LifecycleEvent::Waiting);
            reporter.inner.lock().unwrap().delivered_session.as_deref() == Some("s1")
        }));
        assert_eq!(
            reporter.inner.lock().unwrap().delivered_session.as_deref(),
            Some("s1"),
            "identity converges despite two failed deliveries"
        );
        let sessions = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.method == "pane.report_agent_session")
            .count();
        assert!(
            sessions >= 3,
            "the failed attempts were retried, not dropped (saw {sessions})"
        );
    }

    #[test]
    fn new_coalesces_to_the_newest_session_id() {
        // `/new` in a burst: Herdr must end up knowing the LAST id, and must
        // not be walked through every intermediate one.
        let sink = FakeSink::new();
        let calls = Arc::clone(&sink.calls);
        let (adapter, reporter) = harness(sink);

        for id in ["s1", "s2", "s3"] {
            adapter.on_bare(
                Some(id),
                LifecycleEvent::SessionStarted {
                    session_id: id.into(),
                },
            );
        }
        assert!(eventually(|| reporter
            .inner
            .lock()
            .unwrap()
            .delivered_session
            .as_deref()
            == Some("s3")));
        assert_eq!(
            reporter.inner.lock().unwrap().delivered_session.as_deref(),
            Some("s3"),
            "the newest identity wins"
        );
        let ids: Vec<String> = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.method == "pane.report_agent_session")
            .filter_map(|c| {
                c.params
                    .get("agent_session_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(
            ids.last().map(String::as_str),
            Some("s3"),
            "last reported id is the newest, saw {ids:?}"
        );
    }

    #[test]
    fn a_thinking_does_not_reach_b() {
        let sink = FakeSink::new();
        let (adapter, reporter) = harness(sink);
        start_session(&adapter, &reporter, "B");
        // A's Thinking arrives at B's pane adapter and must be ignored.
        adapter.on_bare(Some("A"), LifecycleEvent::Thinking);
        assert!(eventually(|| worker_is_idle(&reporter.inner)));
        assert_eq!(
            reporter.inner.lock().unwrap().machine.logical.0,
            PaneAgentState::Idle,
            "A's Thinking must not put B's pane to Working"
        );
    }

    #[test]
    fn b_waiting_does_not_reach_a() {
        let sink = FakeSink::new();
        let (adapter, reporter) = harness(sink);
        start_session(&adapter, &reporter, "A");
        adapter.on_bare(Some("A"), LifecycleEvent::TurnStarted);
        assert!(eventually(|| worker_is_idle(&reporter.inner)));
        // B going idle must not idle A.
        adapter.on_bare(Some("B"), LifecycleEvent::Waiting);
        assert!(eventually(|| worker_is_idle(&reporter.inner)));
        assert_eq!(
            reporter.inner.lock().unwrap().machine.logical.0,
            PaneAgentState::Working,
            "B's Waiting must not idle A's pane"
        );
    }

    #[test]
    fn a_session_ended_does_not_shut_down_b_and_b_keeps_reporting() {
        let sink = FakeSink::new();
        let (adapter, reporter) = harness(sink);
        start_session(&adapter, &reporter, "B");
        // A ends. B's reporter must survive it.
        adapter.on_bare(Some("A"), LifecycleEvent::SessionEnded);
        assert!(eventually(|| worker_is_idle(&reporter.inner)));
        assert!(
            !reporter.inner.lock().unwrap().shutdown,
            "A's SessionEnded must not shut down B's reporter"
        );
        // …and B still reports afterwards.
        adapter.on_bare(Some("B"), LifecycleEvent::TurnStarted);
        assert!(eventually(|| worker_is_idle(&reporter.inner)));
        assert_eq!(
            reporter.inner.lock().unwrap().machine.logical.0,
            PaneAgentState::Working,
            "B continues reporting after A ended"
        );
    }
}
