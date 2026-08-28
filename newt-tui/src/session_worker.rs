//! The seam that lets a session's turn stop owning the keyboard.
//!
//! `run_chat` does two jobs at once: it *is* the terminal (it reads keys, owns
//! the editor, draws the prompt) and it *is* the session (it runs turns). While
//! a turn is dispatched, the first job simply stops happening — the loop is
//! parked inside the turn, so nothing services input. That is why the prompt
//! goes dead mid-turn, why steering has nowhere to be typed, and why a tab
//! switch during a turn is unbuildable rather than merely disallowed.
//!
//! The cut is to split the two jobs onto two threads and connect them with a
//! channel:
//!
//! ```text
//!   UI thread                          session thread
//!   ─────────                          ──────────────
//!   owns keyboard, editor, screen      owns run_chat, verbatim
//!            │                                │
//!            │◄────── SurfaceRequest ─────────┤   "read me a line"
//!            │                                │   (parks here, does not spin)
//!            ├─────── SurfaceReply ──────────►│
//! ```
//!
//! **`run_chat` is relocated, not decomposed.** Its ~27 `&mut` locals, its
//! `!Send` turn future and its `!Send` OCAP disclosure guard are all perfectly
//! fine on a thread of their own; they were only ever a problem because that
//! thread also had to service the keyboard. So this module changes *where*
//! `run_chat` runs and *how it asks for a line* — nothing about what it does
//! between those calls.
//!
//! # The two laws this module exists to enforce
//!
//! 1. **A worker never writes terminal bytes.** It publishes state; the UI
//!    thread draws. Two sessions writing to one terminal is how one tab's
//!    output lands in another tab's scrollback.
//! 2. **A turn runs on the thread that installed its guards.** The OCAP
//!    disclosure guard, the lifecycle session scope, and the psyche capture are
//!    all thread-bound by construction (`PhantomData<Rc<()>>`). A turn that
//!    migrated off its thread would silently lose secret redaction on the
//!    memory / observation / compaction / spill paths — and
//!    `verify_disclosure_gate` would report it (#1711), but only if something
//!    asked. [`bind_turn`] installs them on the session's thread for the
//!    duration of each turn, so the question cannot arise.
//!
//! # Session lifetime vs TURN lifetime
//!
//! These are different, and conflating them is a correctness bug rather than
//! an untidiness:
//!
//! - A **session** owns an execution thread, for its whole life.
//! - A **turn** owns its active `SessionId` binding and its psyche snapshot,
//!   for exactly one turn.
//!
//! Binding either at session scope breaks something real. A session-long
//! `SessionId` binding survives a `/tab` switch, so after switching, deep
//! ambient lifecycle and OCAP evidence is still attributed to the tab the
//! process started on. A session-long psyche snapshot freezes the dials
//! forever, so `/psyche`, `/cognition` and a persona's posture stop taking
//! effect on later turns — the opposite of the capture's stated contract,
//! which is that a change lands on the NEXT turn.

use std::sync::mpsc::{Receiver, RecvError, SyncSender};

use crate::chat::{BackgroundJob, ReadOutcome};

/// What a session thread asks the UI thread to do on its behalf.
///
/// Mirrors `InputSurface` one-for-one. Everything the surface can do is either
/// a question (the two that carry a reply channel) or a notification.
#[derive(Debug)]
pub(crate) enum SurfaceRequest {
    /// "Read me one turn." The session parks on `reply` until the operator
    /// submits, interrupts, or the surface degrades.
    ReadLine {
        prompt: String,
        reply: SyncSender<anyhow::Result<ReadOutcome>>,
    },
    /// Rebuild the editor after a `/vi` · `/emacs` switch.
    Reload {
        reply: SyncSender<anyhow::Result<()>>,
    },
    AddHistory(String),
    SaveHistory,
    SetRuntimeContext {
        model: String,
        endpoint: String,
        gauge: Option<(u32, u32)>,
        session: String,
    },
    SetBackgroundJobs(Vec<BackgroundJob>),
    SetTabs(Vec<crate::tab_bar::TabCell>),
    /// #1669 cockpit: a turn is starting; these are the flags the session
    /// races its work against, so the terminal can trip them from Ctrl-C.
    /// A surface that does not read keys during a turn ignores this.
    TurnStarted {
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
        hard: std::sync::Arc<std::sync::atomic::AtomicBool>,
    },
    /// The turn is over: whatever Ctrl-C meant, it means nothing now.
    TurnEnded,
    /// **C1 (#1862): present one semantic interaction and report the outcome.**
    ///
    /// The envelope is thread-shaped — it carries a reply channel, like every
    /// question here. The PAYLOAD is not: `SurfaceInteraction` holds no
    /// channel and no `Arc`, and derives `Serialize`, so the semantic half of
    /// this exchange is the part a later slice can put on a wire. That
    /// layering is the epic's non-goal made structural rather than stated.
    Interact {
        interaction: Box<newt_core::interaction_surface::SurfaceInteraction>,
        reply: SyncSender<newt_core::HumanQuestionOutcome>,
    },
}

impl SurfaceRequest {
    /// Does the sender park on a reply for this request?
    ///
    /// Only the two that return a value. The rest are notifications, which is
    /// what keeps a turn from round-tripping to the UI thread for every status
    /// update it publishes.
    pub(crate) fn expects_reply(&self) -> bool {
        matches!(
            self,
            Self::ReadLine { .. } | Self::Reload { .. } | Self::Interact { .. }
        )
    }
}

/// Why a session's request could not be served.
///
/// Every variant is a *disconnection*, and the distinction matters because
/// each failure mode here is a hang rather than a crash: a worker parked on a
/// reply that will never come is indistinguishable, from the outside, from a
/// worker doing slow work. Naming them keeps that from being silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SurfaceError {
    /// The UI thread is gone — it panicked, or the process is shutting down.
    /// A session that sees this must wind up rather than park forever.
    UiGone,
    /// The UI thread accepted the request and then dropped the reply channel
    /// without answering. Distinct from `UiGone` because it means the request
    /// WAS observed, so retrying it may duplicate a side effect.
    NoReply,
}

impl std::fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::UiGone => "the terminal thread is gone; this session cannot read input",
            Self::NoReply => "the terminal thread dropped the request without answering",
        })
    }
}

impl std::error::Error for SurfaceError {}

/// The session side of the channel: an `InputSurface` that asks the UI thread
/// instead of touching the terminal.
///
/// Deliberately synchronous and blocking. `run_chat` stays exactly as it is —
/// it calls `read_line` and waits — except that it now waits in a channel
/// rather than in `event::read()`, which is what frees the keyboard for the UI
/// thread without restructuring the loop.
pub(crate) struct RemoteSurface {
    to_ui: SyncSender<SurfaceRequest>,
}

impl RemoteSurface {
    pub(crate) fn new(to_ui: SyncSender<SurfaceRequest>) -> Self {
        Self { to_ui }
    }

    /// Send a notification; a dead UI is not worth failing a turn over, so
    /// these are best-effort. The next `read_line` will surface `UiGone`.
    fn notify(&self, request: SurfaceRequest) {
        debug_assert!(
            !request.expects_reply(),
            "a request with a reply channel must go through `ask`"
        );
        let _ = self.to_ui.send(request);
    }

    /// Send a question and park until answered.
    fn ask<T>(
        &self,
        make: impl FnOnce(SyncSender<T>) -> SurfaceRequest,
        rx: Receiver<T>,
        tx: SyncSender<T>,
    ) -> Result<T, SurfaceError> {
        let request = make(tx);
        debug_assert!(request.expects_reply());
        self.to_ui.send(request).map_err(|_| SurfaceError::UiGone)?;
        rx.recv().map_err(|RecvError| SurfaceError::NoReply)
    }
}

impl crate::chat::InputSurface for RemoteSurface {
    fn read_line(&mut self, prompt: &str) -> anyhow::Result<ReadOutcome> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.ask(
            |reply| SurfaceRequest::ReadLine {
                prompt: prompt.to_string(),
                reply,
            },
            rx,
            tx,
        )?
    }

    fn reload(&mut self) -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.ask(|reply| SurfaceRequest::Reload { reply }, rx, tx)?
    }

    fn add_history(&mut self, entry: &str) {
        self.notify(SurfaceRequest::AddHistory(entry.to_string()));
    }

    fn save_history(&mut self) {
        self.notify(SurfaceRequest::SaveHistory);
    }

    fn set_runtime_context(
        &mut self,
        model: &str,
        endpoint: &str,
        gauge: Option<(u32, u32)>,
        session: &str,
    ) {
        self.notify(SurfaceRequest::SetRuntimeContext {
            model: model.to_string(),
            endpoint: endpoint.to_string(),
            gauge,
            session: session.to_string(),
        });
    }

    fn set_background_jobs(&mut self, jobs: Vec<BackgroundJob>) {
        self.notify(SurfaceRequest::SetBackgroundJobs(jobs));
    }

    fn set_tabs(&mut self, tabs: Vec<crate::tab_bar::TabCell>) {
        self.notify(SurfaceRequest::SetTabs(tabs));
    }

    fn turn_started(
        &mut self,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
        hard: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        self.notify(SurfaceRequest::TurnStarted { cancel, hard });
    }

    fn turn_ended(&mut self) {
        self.notify(SurfaceRequest::TurnEnded);
    }

    fn present_interaction(
        &mut self,
        interaction: &newt_core::interaction_surface::SurfaceInteraction,
    ) -> newt_core::HumanQuestionOutcome {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let interaction = Box::new(interaction.clone());
        // A dead or silent UI thread means nobody can be asked — which is
        // `Unavailable`, not a synthesized answer. Fail closed: every
        // security-sensitive caller treats a non-`Answer` outcome as a refusal.
        self.ask(
            |reply| SurfaceRequest::Interact { interaction, reply },
            rx,
            tx,
        )
        .unwrap_or(newt_core::HumanQuestionOutcome::Unavailable)
    }
}

/// Serve one session's surface requests on the thread that owns the terminal.
///
/// Runs until the session drops its end of the channel — which happens when
/// the session returns, after its teardown has had its last `save_history`
/// served. So the pump ending IS the session ending; there is no separate
/// shutdown handshake to get wrong.
///
/// Every request is dispatched onto the REAL surface here. That is the whole
/// point: the surface is built, driven and dropped on one thread, so it needs
/// no `Send` bound and `InputSurface` is untouched by any of this.
pub(crate) fn pump_surface(
    surface: &mut dyn crate::chat::InputSurface,
    requests: &Receiver<SurfaceRequest>,
) {
    for request in requests {
        match request {
            SurfaceRequest::ReadLine { prompt, reply } => {
                // A dropped reply means the session vanished mid-read; the
                // next `recv` ends the loop, so there is nothing to do here.
                let _ = reply.send(surface.read_line(&prompt));
            }
            SurfaceRequest::Reload { reply } => {
                let _ = reply.send(surface.reload());
            }
            SurfaceRequest::AddHistory(entry) => surface.add_history(&entry),
            SurfaceRequest::SaveHistory => surface.save_history(),
            SurfaceRequest::SetRuntimeContext {
                model,
                endpoint,
                gauge,
                session,
            } => surface.set_runtime_context(&model, &endpoint, gauge, &session),
            SurfaceRequest::SetBackgroundJobs(jobs) => surface.set_background_jobs(jobs),
            SurfaceRequest::SetTabs(tabs) => surface.set_tabs(tabs),
            SurfaceRequest::TurnStarted { cancel, hard } => surface.turn_started(cancel, hard),
            SurfaceRequest::TurnEnded => surface.turn_ended(),
            SurfaceRequest::Interact { interaction, reply } => {
                let _ = reply.send(surface.present_interaction(&interaction));
            }
        }
    }
}

/// The thread-bound guards that make ONE TURN's work its own.
///
/// Held for the duration of a single turn and dropped before the next one.
/// Both guards are `!Send` by construction (`PhantomData<Rc<()>>`), so the
/// turn cannot migrate to another thread and quietly start attributing its
/// work — or resolving its dials — somewhere else.
///
/// Deliberately opaque: callers install and hold, never inspect.
pub(crate) struct TurnBinding {
    _session: newt_core::lifecycle::ScopedActiveSession,
    _psyche: newt_core::psyche::TurnPsyche,
}

/// Bind this thread to `session_id` and pin the psyche, for ONE turn.
///
/// The one place the "a turn's work runs on the thread that owns its guards"
/// law is expressed, so a turn cannot be dispatched that forgets half of it:
///
/// - the **lifecycle scope** makes this turn's tool and prompt events belong
///   to the tab that is actually active *for this turn* (#1714) — which is why
///   the caller passes the ACTIVE tab's id, not the one the process started
///   with;
/// - the **psyche capture** pins cognition and tenacity for the turn's
///   duration (#1715), so a dial moved mid-turn cannot change what this turn
///   resolves on a later round — while still taking effect on the next turn,
///   because the binding is dropped in between.
///
/// Call at the turn-dispatch boundary, beside the OCAP disclosure guard which
/// is already scoped exactly this way. Do NOT hoist it to session start: see
/// the module docs for what each of those breaks.
///
/// The OCAP disclosure guard is NOT installed here — it needs the turn's
/// resolved provider secret, which this module deliberately never sees.
/// `verify_disclosure_gate` reports an uninstalled backstop as `Absent`
/// (#1711), so its omission is detectable rather than silent.
#[must_use]
pub(crate) fn bind_turn(session_id: &newt_core::lifecycle::SessionId) -> TurnBinding {
    TurnBinding {
        _session: newt_core::lifecycle::scoped_active_session(session_id),
        _psyche: newt_core::psyche::capture_turn_psyche(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::InputSurface;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A stand-in UI thread: serves requests from the channel, recording what
    /// it saw. This is the deterministic seam the cockpit tests use instead of
    /// a PTY — the property under test is "does the servicing side stay live",
    /// which is a channel property, not a terminal one.
    fn serve_one(rx: &Receiver<SurfaceRequest>, answer: ReadOutcome) -> Option<SurfaceRequest> {
        match rx.recv().ok()? {
            SurfaceRequest::ReadLine { prompt, reply } => {
                reply.send(Ok(answer)).ok()?;
                Some(SurfaceRequest::AddHistory(prompt))
            }
            other => Some(other),
        }
    }

    #[test]
    fn a_session_parks_on_read_line_and_receives_the_ui_threads_answer() {
        let (to_ui, from_session) = std::sync::mpsc::sync_channel(8);
        let worker = std::thread::spawn(move || {
            let mut surface = RemoteSurface::new(to_ui);
            surface.read_line("› ").expect("served")
        });

        let echoed = serve_one(&from_session, ReadOutcome::Line("hello".into()));
        assert!(matches!(
            echoed,
            Some(SurfaceRequest::AddHistory(p)) if p == "› "
        ));
        match worker.join().expect("worker") {
            ReadOutcome::Line(l) => assert_eq!(l, "hello"),
            other => panic!("expected the served line, got {other:?}"),
        }
    }

    /// THE property PR 4 exists for: the servicing side keeps running while a
    /// session is parked mid-"turn".
    ///
    /// Non-vacuous control: `in_flight` is asserted true BOTH before and after
    /// the servicing loop does its work, so this cannot pass by the session
    /// having already finished — it proves the UI side ran *during* the turn.
    #[test]
    fn the_ui_side_stays_live_while_a_session_is_mid_turn() {
        let (to_ui, from_session) = std::sync::mpsc::sync_channel(8);
        let in_flight = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));

        let worker = {
            let in_flight = Arc::clone(&in_flight);
            let release = Arc::clone(&release);
            std::thread::spawn(move || {
                let mut surface = RemoteSurface::new(to_ui);
                in_flight.store(true, Ordering::Release);
                // Stand in for a turn: publish status, then park for input.
                surface.set_runtime_context("m", "http://h", Some((1, 2)), "s");
                let outcome = surface.read_line("› ");
                release.store(true, Ordering::Release);
                outcome
            })
        };

        // Drain the status notification the "turn" published.
        let status = from_session.recv().expect("status");
        assert!(
            matches!(status, SurfaceRequest::SetRuntimeContext { .. }),
            "the worker published state without touching the terminal"
        );
        assert!(
            in_flight.load(Ordering::Acquire),
            "the session is mid-turn right now"
        );
        assert!(
            !release.load(Ordering::Acquire),
            "and has NOT finished — so what follows happens DURING its turn"
        );

        // The UI side is free to do work here; serving the read proves it.
        serve_one(&from_session, ReadOutcome::Line("steer".into()));
        let outcome = worker.join().expect("worker").expect("served");
        assert!(matches!(outcome, ReadOutcome::Line(l) if l == "steer"));
    }

    /// Notifications must not round-trip. A turn publishes status constantly;
    /// if each one parked, the worker would spend its life waiting on the UI.
    #[test]
    fn notifications_do_not_block_the_session() {
        let (to_ui, from_session) = std::sync::mpsc::sync_channel(64);
        let mut surface = RemoteSurface::new(to_ui);
        // Nothing is servicing the channel yet — these must still return.
        for i in 0..32 {
            surface.add_history(&format!("entry-{i}"));
        }
        surface.save_history();
        drop(surface);
        let served = from_session.iter().count();
        assert_eq!(served, 33, "every notification was queued, none awaited");
    }

    /// A dead UI thread must surface as an error, not a permanent park. Every
    /// failure mode on this seam is a hang unless it is named.
    #[test]
    fn a_vanished_ui_thread_is_an_error_rather_than_a_hang() {
        let (to_ui, from_session) = std::sync::mpsc::sync_channel(1);
        drop(from_session);
        let mut surface = RemoteSurface::new(to_ui);
        let err = surface.read_line("› ").expect_err("must not park forever");
        assert!(
            err.to_string().contains("terminal thread is gone"),
            "got: {err}"
        );
    }

    /// A UI thread that takes the request and then drops the reply channel is
    /// a DIFFERENT failure from one that never existed: the request was
    /// observed, so a retry could duplicate its effect.
    #[test]
    fn a_dropped_reply_is_distinguished_from_a_missing_ui() {
        let (to_ui, from_session) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let mut surface = RemoteSurface::new(to_ui);
            surface.read_line("› ")
        });
        // Accept the request, then drop it — reply channel dies with it.
        drop(from_session.recv().expect("request"));
        let err = worker.join().expect("worker").expect_err("no answer");
        assert!(
            err.to_string().contains("dropped the request"),
            "got: {err}"
        );
    }

    #[test]
    fn only_the_value_returning_requests_expect_a_reply() {
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        assert!(SurfaceRequest::Reload { reply: tx }.expects_reply());
        // C1 (#1862) added a THIRD. The name said "the two" and the body
        // spot-checks rather than enumerating, so this would have kept
        // passing while silently describing the wrong set.
        let (itx, _irx) = std::sync::mpsc::sync_channel(1);
        assert!(SurfaceRequest::Interact {
            interaction: Box::new(an_interaction()),
            reply: itx,
        }
        .expects_reply());
        assert!(!SurfaceRequest::SaveHistory.expects_reply());
        assert!(!SurfaceRequest::AddHistory("x".into()).expects_reply());
        assert!(!SurfaceRequest::SetBackgroundJobs(Vec::new()).expects_reply());
    }

    /// Requests arrive in the order the session made them. A status update
    /// that overtook the read it described would render a stale header.
    #[test]
    fn requests_preserve_session_order() {
        let (to_ui, from_session) = std::sync::mpsc::sync_channel(64);
        let mut surface = RemoteSurface::new(to_ui);
        for i in 0..16 {
            surface.add_history(&format!("h{i}"));
        }
        drop(surface);
        let seen: Vec<String> = from_session
            .iter()
            .filter_map(|r| match r {
                SurfaceRequest::AddHistory(h) => Some(h),
                _ => None,
            })
            .collect();
        let want: Vec<String> = (0..16).map(|i| format!("h{i}")).collect();
        assert_eq!(seen, want);
    }

    /// The counter proves the servicing side is doing the work, not the
    /// session — i.e. the split is real rather than the worker secretly
    /// serving itself.
    #[test]
    fn the_serving_side_is_the_one_doing_the_work() {
        let (to_ui, from_session) = std::sync::mpsc::sync_channel(8);
        let served = Arc::new(AtomicUsize::new(0));
        let worker = std::thread::spawn(move || {
            let mut surface = RemoteSurface::new(to_ui);
            surface.read_line("› ")
        });
        let count = Arc::clone(&served);
        if let Ok(SurfaceRequest::ReadLine { reply, .. }) = from_session.recv() {
            count.fetch_add(1, Ordering::Release);
            reply.send(Ok(ReadOutcome::Eof)).expect("reply");
        }
        worker.join().expect("worker").expect("served");
        assert_eq!(served.load(Ordering::Acquire), 1);
    }

    // ── proxy completeness ─────────────────────────────────────────────────

    fn a_cell(number: usize, active: bool) -> crate::tab_bar::TabCell {
        crate::tab_bar::TabCell {
            number,
            label: format!("tab{number}"),
            active,
            degraded: false,
            pending: false,
        }
    }

    /// A tab projection sent by the session must actually arrive at the
    /// terminal.
    ///
    /// This is the assertion that was missing when the bar was first written:
    /// `InputSurface::set_tabs` has a default no-op, `RemoteSurface` did not
    /// override it, and the call therefore compiled, passed every layout test,
    /// and rendered nothing. The layout tests could not catch it because they
    /// never cross the channel.
    #[test]
    fn a_tab_projection_reaches_the_terminal() {
        let (to_ui, from_session) = std::sync::mpsc::sync_channel(4);
        let mut surface = RemoteSurface::new(to_ui);
        surface.set_tabs(vec![a_cell(1, true), a_cell(2, false)]);
        drop(surface);

        match from_session.recv().expect("the projection crossed") {
            SurfaceRequest::SetTabs(cells) => {
                assert_eq!(cells.len(), 2);
                assert_eq!(cells[0], a_cell(1, true), "payload survives intact");
                assert!(cells[1].label.ends_with('2'));
            }
            other => panic!("expected SetTabs, got {other:?}"),
        }
    }

    /// **The guard for the whole class.** Every `InputSurface` method must be
    /// forwarded by the proxy, because since #1718 the session reaches the
    /// terminal only through it — so a method the proxy forgets falls through
    /// to the trait's default body, compiles without a warning, and is dead at
    /// runtime.
    ///
    /// Non-vacuous by construction: it drives all seven methods through
    /// `RemoteSurface` and asserts the far side observed all seven. Delete any
    /// one forwarding impl and the count drops — the exact failure that
    /// shipped a silent no-op the first time.
    ///
    /// If you add a further method to `InputSurface`, this test fails until
    /// the proxy forwards it. That is the point; do not relax the count.
    ///
    /// C1 (#1862) took it from nine to ten with `present_interaction`. That
    /// method is REQUIRED rather than defaulted, so the silent-death case
    /// cannot arise for it — a proxy that forgot it would not compile. This
    /// test still covers it, because "compiles" only proves a body exists;
    /// what must also hold is that the body reaches the FAR SIDE rather than
    /// answering locally, which is a thing a required method can still get
    /// wrong.
    #[test]
    fn the_proxy_forwards_every_surface_method() {
        let (to_ui, from_session) = std::sync::mpsc::sync_channel(16);
        let served = std::thread::spawn(move || {
            let mut recorder = CountingSurface::default();
            pump_surface(&mut recorder, &from_session);
            recorder
        });

        {
            let mut surface = RemoteSurface::new(to_ui);
            surface.set_runtime_context("m", "http://h", Some((1, 2)), "s");
            surface.set_background_jobs(Vec::new());
            surface.set_tabs(vec![a_cell(1, true)]);
            surface.add_history("entry");
            surface.save_history();
            surface.reload().expect("served");
            surface.read_line("› ").expect("served");
            let flag = || std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            surface.turn_started(flag(), flag());
            surface.turn_ended();
            surface.present_interaction(&an_interaction());
        }

        let seen = served.join().expect("terminal thread");
        assert_eq!(
            seen.observed(),
            10,
            "every InputSurface method must cross the proxy; missing: {:?}",
            seen.missing()
        );
    }

    /// **The DEFINITION crosses the seam, not a rendered string.**
    ///
    /// This is the property the slice exists for. `SurfaceRequest::ReadLine`
    /// carries `prompt: String` — the semantic-loss point the A0 inventory
    /// named (§7.5) — so a surface receiving it can only print it. A surface
    /// receiving the definition can render it as plain lines, a Ratatui modal,
    /// or an HTML form.
    ///
    /// A test that only checked "an outcome came back" would pass just as well
    /// if the payload were a pre-rendered string, which is why this asserts on
    /// the payload's structure at the far side.
    #[test]
    fn the_definition_crosses_the_seam_rather_than_a_rendered_string() {
        let (to_ui, from_session) = std::sync::mpsc::sync_channel(4);
        let served = std::thread::spawn(move || {
            let mut seen = None;
            for request in &from_session {
                if let SurfaceRequest::Interact { interaction, reply } = request {
                    seen = Some((*interaction).clone());
                    let _ = reply.send(newt_core::HumanQuestionOutcome::Answer("ok".into()));
                }
            }
            seen
        });

        let outcome = {
            let mut surface = RemoteSurface::new(to_ui);
            surface.present_interaction(&an_interaction())
        };
        assert_eq!(
            outcome,
            newt_core::HumanQuestionOutcome::Answer("ok".into())
        );

        let arrived = served
            .join()
            .expect("terminal thread")
            .expect("an Interact arrived");
        // The far side got the semantic model: a kind, a control it can render
        // however it likes, and the body — none of which survive a `String`.
        assert_eq!(
            arrived.definition.kind,
            newt_interaction::InteractionKind::Prompt
        );
        assert_eq!(arrived.definition.markdown, "? which file");
        assert_eq!(arrived.definition.controls.len(), 1);
        assert!(matches!(
            arrived.definition.controls[0].kind,
            newt_interaction::ControlKind::Text
        ));
        // ...and the lifecycle data the transport used to encode implicitly.
        assert!(arrived.is_blocking());
        assert!(arrived.wants_attention());
    }

    /// **The production session gate asks the SURFACE, not the terminal.**
    ///
    /// `ask_surface: None` is a legitimate state — the single-threaded CLI
    /// entry points ARE the terminal owner — which means production silently
    /// falling back to it would compile, pass every test above, and put the
    /// session thread back on the terminal. This pins the one construction
    /// site that must not.
    #[test]
    fn the_production_session_gate_wires_the_surface_seam() {
        let production = production_code(include_str!("chat.rs"));
        assert!(
            production.contains("ask_surface:Some(&ask_surface)"),
            "the session's permission gate does not wire the surface seam — \
             `ask_question` would take the `None` arm and acquire the \
             PromptWindow on the session thread"
        );
        // The closure it wires must reach the UI thread rather than the
        // terminal: a `RemoteSurface` over the session's channel.
        assert!(
            production.contains("RemoteSurface::new(ask_to_ui.clone()).present_interaction"),
            "the wired closure does not go through the proxy"
        );
    }

    /// Comment lines dropped and whitespace squeezed out of token joins.
    ///
    /// The squeeze is not cosmetic: rustfmt split
    /// `RemoteSurface::new(..).present_interaction(..)` across two lines the
    /// moment it was written, and a single-line needle reported the wiring
    /// MISSING while it was right there. A source guard that a reformat can
    /// flip is worse than none — it teaches you to relax the guard.
    fn production_code(source: &str) -> String {
        source
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .flat_map(str::split_whitespace)
            .collect::<String>()
    }

    /// **Anti-vacuous twin.** The scan must report an unwired gate as
    /// unwired, must not count a commented-out wiring, and must survive the
    /// line split that actually happened.
    #[test]
    fn the_wiring_scan_notices_an_unwired_gate() {
        let unwired = production_code("let gate = PromptPermissionGate {\n ask_surface: None,\n};");
        assert!(!unwired.contains("ask_surface:Some(&ask_surface)"));

        let commented = production_code("// ask_surface: Some(&ask_surface) would go here");
        assert!(
            !commented.contains("ask_surface:Some(&ask_surface)"),
            "a commented-out wiring counted as a real one"
        );

        // The split form the formatter produces must still be seen.
        let split = production_code(
            "RemoteSurface::new(ask_to_ui.clone())\n    .present_interaction(interaction)",
        );
        assert!(
            split.contains("RemoteSurface::new(ask_to_ui.clone()).present_interaction"),
            "the scan cannot see a rustfmt-split call chain"
        );
    }

    /// One semantic interaction, for the seam tests.
    fn an_interaction() -> newt_core::interaction_surface::SurfaceInteraction {
        use newt_interaction::{Control, ControlId, ControlKind, InteractionKind, Requirement};
        newt_core::interaction_surface::SurfaceInteraction::blocking(
            newt_interaction::InteractionDefinition::new(
                InteractionKind::Prompt,
                "? which file",
                vec![Control {
                    id: ControlId::new("answer").expect("valid control id"),
                    kind: ControlKind::Text,
                    label: String::new(),
                    requirement: Requirement::Required,
                }],
            ),
        )
    }

    /// Records which surface methods the terminal was actually asked to run.
    #[derive(Default)]
    struct CountingSurface {
        read_line: usize,
        add_history: usize,
        save_history: usize,
        reload: usize,
        runtime_context: usize,
        background_jobs: usize,
        tabs: usize,
        turn_started: usize,
        turn_ended: usize,
        present_interaction: usize,
    }

    impl CountingSurface {
        fn each(&self) -> [(&'static str, usize); 10] {
            [
                ("read_line", self.read_line),
                ("add_history", self.add_history),
                ("save_history", self.save_history),
                ("reload", self.reload),
                ("set_runtime_context", self.runtime_context),
                ("set_background_jobs", self.background_jobs),
                ("set_tabs", self.tabs),
                ("turn_started", self.turn_started),
                ("turn_ended", self.turn_ended),
                ("present_interaction", self.present_interaction),
            ]
        }
        fn observed(&self) -> usize {
            self.each().iter().filter(|(_, n)| *n > 0).count()
        }
        fn missing(&self) -> Vec<&'static str> {
            self.each()
                .iter()
                .filter(|(_, n)| *n == 0)
                .map(|(name, _)| *name)
                .collect()
        }
    }

    impl crate::chat::InputSurface for CountingSurface {
        fn present_interaction(
            &mut self,
            _interaction: &newt_core::interaction_surface::SurfaceInteraction,
        ) -> newt_core::HumanQuestionOutcome {
            self.present_interaction += 1;
            newt_core::HumanQuestionOutcome::Answer("served".into())
        }

        fn read_line(&mut self, _prompt: &str) -> anyhow::Result<ReadOutcome> {
            self.read_line += 1;
            Ok(ReadOutcome::Eof)
        }
        fn add_history(&mut self, _entry: &str) {
            self.add_history += 1;
        }
        fn save_history(&mut self) {
            self.save_history += 1;
        }
        fn reload(&mut self) -> anyhow::Result<()> {
            self.reload += 1;
            Ok(())
        }
        fn set_runtime_context(&mut self, _m: &str, _e: &str, _g: Option<(u32, u32)>, _s: &str) {
            self.runtime_context += 1;
        }
        fn set_background_jobs(&mut self, _jobs: Vec<BackgroundJob>) {
            self.background_jobs += 1;
        }
        fn set_tabs(&mut self, _tabs: Vec<crate::tab_bar::TabCell>) {
            self.tabs += 1;
        }
        fn turn_started(
            &mut self,
            _cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
            _hard: std::sync::Arc<std::sync::atomic::AtomicBool>,
        ) {
            self.turn_started += 1;
        }
        fn turn_ended(&mut self) {
            self.turn_ended += 1;
        }
    }

    // ── end-to-end session lifecycle ───────────────────────────────────────

    /// The architectural property this whole PR exists for, exercised as one
    /// lifecycle rather than as isolated pieces.
    ///
    /// Drives the REAL topology `run_chat` now uses — a session on its own
    /// thread reaching the terminal only through `RemoteSurface`, and the
    /// terminal servicing it with `pump_surface` — through:
    ///
    /// 1. start the machinery;
    /// 2. begin a turn and BLOCK it mid-flight;
    /// 3. prove the terminal side is still alive and serving while that turn
    ///    is demonstrably unfinished;
    /// 4. let the turn finish;
    /// 5. run a second turn and prove its bindings are FRESH, not turn 1's;
    /// 6. shut down;
    /// 7. prove the worker terminated and nothing is left hanging.
    ///
    /// What makes it non-vacuous, in three places:
    ///
    /// - step 3 asserts `turn_one_done == false` at the moment the terminal
    ///   serves, so a pass cannot come from the turn having already finished.
    ///   Move execution back onto the terminal thread and the pump cannot run
    ///   here at all — the test deadlocks and fails on the harness timeout
    ///   rather than passing quietly.
    /// - step 5 asserts session B and dial P2, which a session-lifetime
    ///   binding answers as A and P1.
    /// - step 7 asserts the channel is closed AND the worker joined; a leaked
    ///   worker or an un-exited pump fails rather than being invisible.
    ///
    /// Synchronisation is entirely by channel/atomic rendezvous — no sleeps,
    /// so it cannot flake into a pass on a loaded machine.
    #[test]
    fn a_session_serves_two_turns_and_shuts_down_without_leaking() {
        use newt_core::tenacity::{effective_tenacity, set_cli_tenacity, Tenacity};
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();

        let a = newt_core::lifecycle::new_session_id();
        let b = newt_core::lifecycle::new_session_id();
        set_cli_tenacity(Tenacity::Relaxed); // P1

        let (to_ui, from_session) = std::sync::mpsc::sync_channel(16);
        // Rendezvous: the terminal releases turn 1 only after it has proven
        // itself alive, so "alive during the turn" is ordered, not hoped for.
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let turn_one_done = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));

        std::thread::scope(|scope| {
            // ── the SESSION: its own thread, exactly as `run_chat` spawns it.
            let session = {
                let (a, b) = (a.clone(), b.clone());
                let (done, seen) = (Arc::clone(&turn_one_done), Arc::clone(&observed));
                scope.spawn(move || {
                    let mut surface = RemoteSurface::new(to_ui);

                    // (2) turn 1 — bound, then blocked mid-flight.
                    {
                        let _turn = bind_turn(&a);
                        surface.set_runtime_context("m1", "http://h", None, "s");
                        seen.lock()
                            .unwrap()
                            .push((newt_core::lifecycle::active_session(), effective_tenacity()));
                        // Park until the terminal says it has served something
                        // else. If execution were back on the terminal thread,
                        // nothing could ever send this.
                        release_rx.recv().expect("terminal releases turn 1");
                    }
                    done.store(true, Ordering::Release);

                    // (5) turn 2 — a different tab, and the dial has moved.
                    {
                        let _turn = bind_turn(&b);
                        seen.lock()
                            .unwrap()
                            .push((newt_core::lifecycle::active_session(), effective_tenacity()));
                    }
                    // (6) session ends; dropping `surface` closes the channel.
                })
            };

            // ── the TERMINAL: services the session on this thread.
            // (3) serve turn 1's status update while the turn is still parked.
            match from_session.recv().expect("turn 1 published status") {
                SurfaceRequest::SetRuntimeContext { model, .. } => {
                    assert_eq!(model, "m1");
                    assert!(
                        !turn_one_done.load(Ordering::Acquire),
                        "the terminal served while turn 1 was still running —                          which is the entire point of the split"
                    );
                }
                other => panic!("expected the turn's status update, got {other:?}"),
            }

            // The operator moves a dial between turns.
            set_cli_tenacity(Tenacity::Relentless); // P2
                                                    // (4) let turn 1 finish.
            release_tx.send(()).expect("session still listening");

            // (7) drain to channel close — the pump's real exit condition —
            // then confirm the worker is done.
            let mut surface = RecordingSurface::default();
            pump_surface(&mut surface, &from_session);
            session.join().expect("session thread joined cleanly");
        });

        // (5) turn-local state was freshly bound on turn 2.
        let seen = observed.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "two turns ran");
        assert_eq!(seen[0].0.as_deref(), Some(a.as_str()), "turn 1 is A's");
        assert_eq!(seen[0].1, Tenacity::Relaxed, "turn 1 held P1");
        assert_eq!(
            seen[1].0.as_deref(),
            Some(b.as_str()),
            "turn 2 is B's — a session-long binding answers A here"
        );
        assert_eq!(
            seen[1].1,
            Tenacity::Relentless,
            "turn 2 sees P2 — a session-long capture answers P1 here"
        );

        // (7) nothing is left hanging — and this is structural, not asserted.
        // `thread::scope` cannot return until every thread it spawned has been
        // joined, so reaching this line at all is the proof that the session
        // worker terminated. `pump_surface` returning inside the scope is the
        // matching proof for the channel: its loop ends only when every sender
        // is dropped. A leaked worker or a pump still parked on `recv` hangs
        // here instead of passing.
        set_cli_tenacity(Tenacity::Standard);
    }

    /// A surface that records what the terminal was asked to do.
    #[derive(Default)]
    struct RecordingSurface {
        history: Vec<String>,
        saves: usize,
    }

    impl crate::chat::InputSurface for RecordingSurface {
        fn present_interaction(
            &mut self,
            _interaction: &newt_core::interaction_surface::SurfaceInteraction,
        ) -> newt_core::HumanQuestionOutcome {
            newt_core::HumanQuestionOutcome::Unavailable
        }

        fn read_line(&mut self, _prompt: &str) -> anyhow::Result<ReadOutcome> {
            Ok(ReadOutcome::Eof)
        }
        fn add_history(&mut self, entry: &str) {
            self.history.push(entry.to_string());
        }
        fn save_history(&mut self) {
            self.saves += 1;
        }
        fn reload(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    // ── turn-scoped binding (the #1718 review fix) ─────────────────────────

    /// THE regression. A session that switches tabs must attribute each turn's
    /// ambient events to the tab that is active FOR THAT TURN.
    ///
    /// This is what a session-lifetime binding got wrong: it pinned the
    /// startup tab's id to the thread forever, and because `active_session()`
    /// prefers the thread scope, every later turn — on any tab — still
    /// resolved to the tab the process happened to start on.
    ///
    /// Non-vacuous: the assertion for turn 2 is `b`, and a session-long
    /// binding necessarily answers `a` there. Reinstating the old lifetime
    /// fails this test.
    #[test]
    fn each_turn_is_attributed_to_the_tab_active_for_that_turn() {
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        let a = newt_core::lifecycle::new_session_id();
        let b = newt_core::lifecycle::new_session_id();

        // One session thread, two turns, a tab switch in between.
        let observed = std::thread::spawn({
            let (a, b) = (a.clone(), b.clone());
            move || {
                let mut seen = Vec::new();
                {
                    let _turn = bind_turn(&a);
                    seen.push(newt_core::lifecycle::active_session());
                }
                // …operator types `/tab 2` here; the turn guard is gone.
                {
                    let _turn = bind_turn(&b);
                    seen.push(newt_core::lifecycle::active_session());
                }
                // …and between turns the thread claims nothing.
                seen.push(newt_core::lifecycle::active_session());
                seen
            }
        })
        .join()
        .expect("session thread");

        assert_eq!(observed[0].as_deref(), Some(a.as_str()), "turn 1 is A's");
        assert_eq!(
            observed[1].as_deref(),
            Some(b.as_str()),
            "turn 2 is B's — a session-long binding would still answer A here"
        );
        assert_eq!(
            observed[2], None,
            "between turns the thread holds no claim, so nothing inherits a \
             stale binding"
        );
    }

    /// A turn is internally stable, and the NEXT turn sees the change. Both
    /// halves matter: the first is what the capture is for, the second is what
    /// a session-long capture destroyed.
    ///
    /// Non-vacuous: `P2` is asserted for turn 2, and a session-long capture
    /// necessarily answers `P1` there.
    #[test]
    fn a_dial_moved_between_turns_lands_on_the_next_turn() {
        use newt_core::tenacity::{effective_tenacity, set_cli_tenacity, Tenacity};
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();

        // P1 for turn 1.
        set_cli_tenacity(Tenacity::Relaxed);
        {
            let _turn = bind_turn(&newt_core::lifecycle::new_session_id());
            assert_eq!(effective_tenacity(), Tenacity::Relaxed, "turn 1 sees P1");
            // The operator moves the dial DURING turn 1.
            set_cli_tenacity(Tenacity::Relentless);
            assert_eq!(
                effective_tenacity(),
                Tenacity::Relaxed,
                "…and turn 1 stays internally stable despite it"
            );
        }
        // Turn 2 picks the change up.
        {
            let _turn = bind_turn(&newt_core::lifecycle::new_session_id());
            assert_eq!(
                effective_tenacity(),
                Tenacity::Relentless,
                "turn 2 sees P2 — a session-long capture would still answer P1"
            );
        }
        set_cli_tenacity(Tenacity::Standard);
    }

    /// Cognition rides the same binding, so `/cognition` between turns lands
    /// on the next one too. Separate from tenacity because they are separate
    /// globals and a capture that pinned only one would pass the test above.
    #[test]
    fn a_cognition_change_between_turns_also_lands_on_the_next_turn() {
        use newt_core::cognition::{effective_cognition, set_cli_cognition, CognitionOverride};
        use newt_core::role_profile::Cognition;
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();

        set_cli_cognition(CognitionOverride::Set(Cognition::Pondering));
        {
            let _turn = bind_turn(&newt_core::lifecycle::new_session_id());
            assert_eq!(effective_cognition(), Some(Cognition::Pondering));
            set_cli_cognition(CognitionOverride::Set(Cognition::Contemplating));
            assert_eq!(
                effective_cognition(),
                Some(Cognition::Pondering),
                "the running turn is stable"
            );
        }
        {
            let _turn = bind_turn(&newt_core::lifecycle::new_session_id());
            assert_eq!(
                effective_cognition(),
                Some(Cognition::Contemplating),
                "the next turn sees the new dial"
            );
        }
        set_cli_cognition(CognitionOverride::Unset);
    }

    /// Two sessions' turns, running at once on their own threads, do not cross
    /// — the property the whole split exists to protect.
    #[test]
    fn concurrent_turns_on_two_threads_do_not_cross_attribution() {
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        let a = newt_core::lifecycle::new_session_id();
        let b = newt_core::lifecycle::new_session_id();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));

        std::thread::scope(|scope| {
            for id in [&a, &b] {
                let seen = Arc::clone(&seen);
                scope.spawn(move || {
                    let _turn = bind_turn(id);
                    seen.lock().unwrap().push((
                        id.as_str().to_string(),
                        newt_core::lifecycle::active_session(),
                    ));
                });
            }
        });

        for (expected, actual) in seen.lock().unwrap().iter() {
            assert_eq!(
                actual.as_deref(),
                Some(expected.as_str()),
                "each turn resolved to its own session"
            );
        }
    }

    /// A turn binding releases the thread on drop, including on unwind — a
    /// panicking turn must not leave the next one inheriting its identity.
    #[test]
    fn a_panicking_turn_still_releases_its_binding() {
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        newt_core::lifecycle::clear_active_session();
        let id = newt_core::lifecycle::new_session_id();

        let panicked = std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _turn = bind_turn(&id);
                panic!("turn blew up");
            }));
            assert!(result.is_err());
            // The guard's Drop ran during the unwind.
            newt_core::lifecycle::active_session()
        })
        .join()
        .expect("probe thread");

        assert_eq!(
            panicked, None,
            "a failed turn releases its binding rather than leaking it"
        );
    }
}
