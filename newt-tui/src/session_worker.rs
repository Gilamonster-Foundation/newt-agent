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
//!    asked. [`SessionWorker::spawn`] installs all three at the top of the
//!    thread and holds them for its whole life, so the question cannot arise.

// DELIBERATELY UNWIRED IN THIS SLICE. `run_chat` is not yet relocated onto a
// worker, so nothing constructs these yet and every item below reads as dead.
// The seam lands first, with its tests, so the relocation that follows is a
// mechanical move against a proven channel rather than one large change that
// invents the protocol and moves 5,000 lines at the same time. Delete this
// attribute in the relocation slice — if it survives past that, the seam was
// never wired and this module is genuinely dead.
#![allow(dead_code)]

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
}

impl SurfaceRequest {
    /// Does the sender park on a reply for this request?
    ///
    /// Only the two that return a value. The rest are notifications, which is
    /// what keeps a turn from round-tripping to the UI thread for every status
    /// update it publishes.
    pub(crate) fn expects_reply(&self) -> bool {
        matches!(self, Self::ReadLine { .. } | Self::Reload { .. })
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

    pub(crate) fn read_line(&mut self, prompt: &str) -> anyhow::Result<ReadOutcome> {
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

    pub(crate) fn reload(&mut self) -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.ask(|reply| SurfaceRequest::Reload { reply }, rx, tx)?
    }

    pub(crate) fn add_history(&mut self, entry: &str) {
        self.notify(SurfaceRequest::AddHistory(entry.to_string()));
    }

    pub(crate) fn save_history(&mut self) {
        self.notify(SurfaceRequest::SaveHistory);
    }

    pub(crate) fn set_runtime_context(
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

    pub(crate) fn set_background_jobs(&mut self, jobs: Vec<BackgroundJob>) {
        self.notify(SurfaceRequest::SetBackgroundJobs(jobs));
    }
}

/// A session running on its own thread, with its thread-bound guards
/// installed for the whole of its life.
///
/// The join handle is the session; dropping this without joining detaches it,
/// which is why `join` is the only way to retire one.
pub(crate) struct SessionWorker {
    session_id: newt_core::lifecycle::SessionId,
    handle: std::thread::JoinHandle<()>,
}

impl SessionWorker {
    /// Run `body` on a thread that belongs to `session_id`.
    ///
    /// This is the **one** place the "a turn runs on the thread that installed
    /// its guards" law is expressed, and it is expressed by construction: the
    /// guards are installed here, before `body` is called, and held until it
    /// returns. `body` cannot observe a thread without them and cannot move
    /// them (they are `!Send` by `PhantomData<Rc<()>>`).
    ///
    /// Two of the three matter for correctness rather than tidiness:
    ///
    /// - the **lifecycle scope** is what makes this session's tool and prompt
    ///   events its own, so a background session's activity is never
    ///   attributed to whichever tab happens to be visible (#1714);
    /// - the **psyche capture** pins cognition and tenacity, so an operator
    ///   moving a dial in another tab cannot change what this turn resolves
    ///   mid-flight (#1715).
    ///
    /// The OCAP disclosure guard is installed by the caller inside `body`,
    /// because it needs the session's resolved provider secret — which this
    /// module deliberately never sees. `verify_disclosure_gate` reports an
    /// uninstalled backstop as `Absent` (#1711), so the omission is detectable
    /// rather than silent.
    pub(crate) fn spawn(
        session_id: newt_core::lifecycle::SessionId,
        body: impl FnOnce() + Send + 'static,
    ) -> Self {
        let owned = session_id.clone();
        let handle = std::thread::spawn(move || {
            let _session = newt_core::lifecycle::scoped_active_session(&owned);
            let _psyche = newt_core::psyche::capture_turn_psyche();
            body();
        });
        Self { session_id, handle }
    }

    pub(crate) fn session_id(&self) -> &newt_core::lifecycle::SessionId {
        &self.session_id
    }

    /// Has this session finished? Non-blocking, so the UI thread can reap
    /// without ever parking on a worker.
    pub(crate) fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Wait for this session to end.
    pub(crate) fn join(self) -> std::thread::Result<()> {
        self.handle.join()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn only_the_two_value_returning_requests_expect_a_reply() {
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        assert!(SurfaceRequest::Reload { reply: tx }.expects_reply());
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

    // ── SessionWorker: the guards are installed by construction ────────────

    /// Two sessions running at once each see their OWN identity, and neither
    /// can be attributed to the other. This is the whole point of installing
    /// the lifecycle scope on the worker rather than trusting a process cell.
    #[test]
    fn each_worker_runs_under_its_own_session_identity() {
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        let a = newt_core::lifecycle::new_session_id();
        let b = newt_core::lifecycle::new_session_id();
        let seen_a = Arc::new(std::sync::Mutex::new(None));
        let seen_b = Arc::new(std::sync::Mutex::new(None));

        let (wa, wb) = {
            let (sa, sb) = (Arc::clone(&seen_a), Arc::clone(&seen_b));
            (
                SessionWorker::spawn(a.clone(), move || {
                    *sa.lock().unwrap() = newt_core::lifecycle::active_session();
                }),
                SessionWorker::spawn(b.clone(), move || {
                    *sb.lock().unwrap() = newt_core::lifecycle::active_session();
                }),
            )
        };
        assert_eq!(wa.session_id(), &a);
        assert_eq!(wb.session_id(), &b);
        wa.join().expect("a");
        wb.join().expect("b");

        assert_eq!(seen_a.lock().unwrap().as_deref(), Some(a.as_str()));
        assert_eq!(seen_b.lock().unwrap().as_deref(), Some(b.as_str()));
    }

    /// The psyche capture is installed BEFORE the body runs, so a dial moved
    /// after the worker started cannot change what its turn resolves.
    ///
    /// Non-vacuous: the body waits until the test has actually mutated the
    /// dial, so a missing capture would observe the new value.
    #[test]
    fn a_workers_psyche_is_captured_before_its_body_runs() {
        use newt_core::tenacity::{effective_tenacity, set_cli_tenacity, Tenacity};
        let _g = newt_core::test_guard::GlobalSettingsGuard::acquire();
        set_cli_tenacity(Tenacity::Relaxed);

        // The handshake is load-bearing: `spawn` installs the capture BEFORE
        // it calls the body, so the body running at all proves the capture is
        // already in place. Without waiting for that, the mutation below could
        // land first and the worker would legitimately capture the NEW value —
        // a race in the test, not a defect in the capture.
        let captured = Arc::new(AtomicBool::new(false));
        let mutated = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(std::sync::Mutex::new(None));
        let worker = {
            let (c, m, o) = (
                Arc::clone(&captured),
                Arc::clone(&mutated),
                Arc::clone(&observed),
            );
            SessionWorker::spawn(newt_core::lifecycle::new_session_id(), move || {
                c.store(true, Ordering::Release);
                while !m.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                *o.lock().unwrap() = Some(effective_tenacity());
            })
        };

        while !captured.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        set_cli_tenacity(Tenacity::Relentless);
        mutated.store(true, Ordering::Release);
        worker.join().expect("worker");

        assert_eq!(
            *observed.lock().unwrap(),
            Some(Tenacity::Relaxed),
            "the worker kept the dial it started with, despite the mid-flight change"
        );
        set_cli_tenacity(Tenacity::Standard);
    }

    /// A worker that has ended reports so without blocking — the UI thread
    /// must be able to reap without ever parking on a session.
    #[test]
    fn a_finished_worker_is_observable_without_joining() {
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        let worker = SessionWorker::spawn(newt_core::lifecycle::new_session_id(), move || {
            flag.store(true, Ordering::Release);
        });
        while !worker.is_finished() {
            std::hint::spin_loop();
        }
        assert!(done.load(Ordering::Acquire));
        worker.join().expect("worker");
    }
}
