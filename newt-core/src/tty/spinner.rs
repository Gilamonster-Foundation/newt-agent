//! The ONE spinner, driven by the ONE ticker.
//!
//! # Why a dedicated OS thread and not a tokio task
//!
//! Both spinners this replaces went visibly dead exactly when liveness mattered
//! most:
//!
//! - the reasoning spinner advanced *only* when a reasoning chunk arrived, so a
//!   model stall froze both the glyph and the clock — precisely the "looks
//!   hung" signature that motivated this work;
//! - the `tokio::select!` spinner shared an executor thread with the future it
//!   covered, so any synchronous blocking inside that future starved the ticker
//!   and froze the animation identically.
//!
//! A wall-clock ticker on its own OS thread is immune to both. The spinner is
//! now alive *especially* when the thing it covers is stuck, which is the whole
//! point of a spinner.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use crossterm::queue;
use crossterm::style::{Color as CtColor, Print, ResetColor, SetForegroundColor};

use super::arbiter::{Ephemeral, LineLease, Sink, Terminal};
use super::caps::LineCaps;
use super::{fit_line, format_spinner, term_cols, FADE_CT};

/// The shared frame cadence. 100 ms unifies the three clocks this replaces
/// (event-driven, `interval(120ms)`, and `poll/sleep(100ms)`); 120 ms was the
/// odd one out, so this is the smallest visual delta available.
const TICK: Duration = Duration::from_millis(100);

/// Process-wide "the user asked to interrupt" signal.
///
/// Set by the TUI's keyboard watcher the moment Esc/Ctrl-C trips the graceful
/// cancel flag, cleared when the turn ends. The spinner reads it on every
/// frame and swaps its stage label for [`INTERRUPT_LABEL`], so the press is
/// acknowledged on screen within one tick (~100 ms) — through the line the
/// spinner already owns, never a second terminal writer (the #1312 rule).
/// Without this, a graceful cancel is invisible until the turn reaches its
/// next checkpoint and the whole TUI reads as hung.
static INTERRUPT_PENDING: AtomicBool = AtomicBool::new(false);

/// The acknowledgment label shown in place of the stage label while an
/// interrupt is pending.
pub const INTERRUPT_LABEL: &str = "interrupting… (press Ctrl-C again to force)";

/// Flag/clear the pending-interrupt acknowledgment. The TUI watcher sets it on
/// the first Esc/Ctrl-C; the turn wrapper clears it when the turn hands back.
pub fn set_interrupt_pending(on: bool) {
    INTERRUPT_PENDING.store(on, Ordering::SeqCst);
}

/// Whether an interrupt acknowledgment is currently pending.
pub fn interrupt_pending() -> bool {
    INTERRUPT_PENDING.load(Ordering::SeqCst)
}

/// The stage label a frame should actually show: the caller's label normally,
/// the interrupt acknowledgment while a cancel is pending. Pure for testing.
fn effective_label(label: &str, interrupted: bool) -> &str {
    if interrupted {
        INTERRUPT_LABEL
    } else {
        label
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct SpinnerState {
    lease: LineLease,
    start: Instant,
    label: Mutex<String>,
    /// Characters of detail seen so far — drives the `· N chars` tail.
    chars: AtomicUsize,
    /// Partial detail line awaiting its newline.
    line_buf: Mutex<String>,
    frame: AtomicUsize,
    /// Styling ONLY. Never a capability signal — see [`LineCaps`].
    color: bool,
    finished: AtomicBool,
    /// Serializes `draw` against `finish`. Without it a tick could pass the
    /// `finished` check, lose the CPU, and paint AFTER `finish` had erased —
    /// a stale row that the lease's final erase would then wipe from wherever
    /// the cursor had moved to. Harmless when the next writer is a permanent
    /// line on a fresh row; destructive when it is a viewport that has just
    /// taken this row (#1727's spinner → live-output hand-off). Lock order is
    /// always gate → stdout, in both holders.
    paint_gate: Mutex<()>,
}

impl SpinnerState {
    /// Draw the current frame. Always width-fitted: the row is redrawn in place
    /// and never scrolls, so a line wider than the terminal would wrap and leave
    /// stale rows behind that no single-line erase can reach.
    fn draw(&self) {
        let _gate = self
            .paint_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.finished.load(Ordering::SeqCst) {
            return;
        }
        let label = self
            .label
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let line = format_spinner(
            self.frame.load(Ordering::SeqCst),
            self.start.elapsed().as_secs_f32(),
            effective_label(&label, interrupt_pending()),
            self.chars.load(Ordering::SeqCst),
        );
        let fitted = fit_line(&line, term_cols());
        let color = self.color;
        self.lease.paint(move |w| {
            if color {
                queue!(
                    w,
                    SetForegroundColor(CtColor::DarkGrey),
                    Print(&fitted.head),
                    SetForegroundColor(FADE_CT),
                    Print(&fitted.fade),
                    Print(fitted.ellipsis),
                    ResetColor,
                )?;
            } else {
                write!(w, "{}{}{}", fitted.head, fitted.fade, fitted.ellipsis)?;
            }
            Ok(())
        });
    }

    fn tick(&self) {
        self.frame.fetch_add(1, Ordering::SeqCst);
        self.draw();
    }

    /// Erase and stop. Idempotent — `Drop` and an explicit `finish()` are the
    /// same operation, so teardown can never double-erase a row someone else
    /// has since taken.
    fn finish(&self) {
        let _gate = self
            .paint_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        // Flush a trailing partial detail line so nothing the model said is lost
        // to the erase.
        let tail = {
            let mut buf = self
                .line_buf
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *buf)
        };
        let trimmed = tail.trim_end_matches(['\n', '\r']);
        if !trimmed.trim().is_empty() {
            self.emit_detail_line(trimmed);
        }
        self.lease.erase();
    }

    /// Write one completed detail line into scrollback (dim), below/behind the
    /// ephemeral row.
    fn emit_detail_line(&self, line: &str) {
        let color = self.color;
        let owned = line.to_string();
        self.lease.emit_line(move |w| {
            if color {
                queue!(
                    w,
                    SetForegroundColor(CtColor::DarkGrey),
                    Print("  "),
                    Print(&owned),
                    ResetColor,
                    Print("\n"),
                )?;
            } else {
                writeln!(w, "  {owned}")?;
            }
            Ok(())
        });
    }
}

impl Ephemeral for SpinnerState {
    /// Erase through the SAME [`SpinnerState::paint_gate`] as `draw`/`finish`.
    ///
    /// The suspend path is "set `suspended`, THEN erase every ephemeral"
    /// (`Terminal::suspend_for_prompt`). Without the gate that leaves a live
    /// window: a tick can pass `LineLease::paint`'s `suspended()` check while
    /// the flag is still clear, lose the CPU, and flush its frame *after* the
    /// erase — repainting the row the question is about to occupy. That is
    /// the invisible-prompt hang the arbiter exists to end, arriving through
    /// the one door it did not close.
    ///
    /// Taking the gate makes the two orderings the only two possible:
    ///
    /// - the tick was already inside `draw` → this erase waits for it, then
    ///   clears what it wrote (`painted` is set, so the erase is real);
    /// - the tick arrives after → it takes the gate next, and `paint` finds
    ///   `suspended()` true and writes nothing.
    ///
    /// Lock order is `paint_gate` → stdout, matching `draw` and `finish`.
    /// Both callers of this method — `Terminal::suspend_for_prompt` and
    /// `Terminal::emit_line` — collect the registered ephemerals under the
    /// arbiter mutex and **release it before erasing**, so no thread ever
    /// holds the arbiter mutex while waiting on this gate and there is no
    /// cycle to deadlock on.
    fn erase(&self) {
        let _gate = self
            .paint_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.lease.erase();
    }
    fn restore(&self) {
        // Nothing to do: the shared ticker repaints within one frame, and
        // repainting here would race the question's own final flush.
    }
}

// ---------------------------------------------------------------------------
// The ticker
// ---------------------------------------------------------------------------

type Registry = (Mutex<Vec<Weak<SpinnerState>>>, Condvar);

fn ticker() -> &'static Registry {
    static TICKER: OnceLock<Registry> = OnceLock::new();
    TICKER.get_or_init(|| {
        let reg: Registry = (Mutex::new(Vec::new()), Condvar::new());
        reg
    })
}

/// Spawn the single ticker thread, once per process. It sleeps on the condvar
/// whenever nothing is registered, so an idle process pays nothing for it.
fn ensure_ticker_thread() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::Builder::new()
            .name("newt-tty-ticker".to_string())
            .spawn(|| loop {
                let (m, cv) = ticker();
                let live: Vec<Arc<SpinnerState>> = {
                    let mut list = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    while list.is_empty() {
                        list = cv
                            .wait(list)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                    list.retain(|w| w.strong_count() > 0);
                    list.iter().filter_map(Weak::upgrade).collect()
                };
                for s in &live {
                    s.tick();
                }
                // Release every strong reference BEFORE sleeping: a `Spinner`
                // dropped mid-frame must erase immediately, not one tick later.
                drop(live);
                std::thread::sleep(TICK);
            })
            .ok();
    });
}

fn register(state: &Arc<SpinnerState>) {
    ensure_ticker_thread();
    let (m, cv) = ticker();
    m.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(Arc::downgrade(state));
    cv.notify_all();
}

// ---------------------------------------------------------------------------
// The handle
// ---------------------------------------------------------------------------

/// The single spinner. Ticking requires the lease, so holding one of these IS
/// ownership of the terminal's bottom line.
pub struct Spinner {
    state: Arc<SpinnerState>,
}

impl Spinner {
    /// Start a spinner, or return `None` when this process may not own a line —
    /// in which case **zero bytes are emitted**, on any stream.
    pub fn start(label: &str, sink: Sink, color: bool) -> Option<Self> {
        Self::start_with_caps(super::caps::detect(), label, sink, color)
    }

    /// [`Spinner::start`] with the capability supplied rather than detected.
    ///
    /// The migration seam (see [`Terminal::lease_with_caps`]): a caller that
    /// already computed its own legacy gate keeps deciding *when* a spinner
    /// appears, so a spinner can move onto the arbiter with no behavior change.
    /// Protocol mode still vetoes absolutely. New code should call
    /// [`Spinner::start`].
    pub fn start_with_caps(caps: LineCaps, label: &str, sink: Sink, color: bool) -> Option<Self> {
        let lease = Terminal::lease_with_caps(caps, sink)?;
        let state = Arc::new(SpinnerState {
            lease,
            start: Instant::now(),
            label: Mutex::new(label.to_string()),
            chars: AtomicUsize::new(0),
            line_buf: Mutex::new(String::new()),
            frame: AtomicUsize::new(0),
            color,
            finished: AtomicBool::new(false),
            paint_gate: Mutex::new(()),
        });
        let as_ephemeral: Arc<dyn Ephemeral> = state.clone();
        Terminal::register(
            Arc::as_ptr(&state) as *const () as usize as u64,
            &as_ephemeral,
        );
        register(&state);
        Some(Self { state })
    }

    /// Change the stage text without resetting the clock
    /// (`thinking…` → `compressing context…`).
    pub fn set_label(&self, label: &str) {
        let mut cur = self
            .state
            .label
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *cur = label.to_string();
    }

    /// Feed a detail chunk (the model's reasoning trickle): completed lines are
    /// flushed to scrollback as dim text, and the counter feeding the
    /// `· N chars` tail advances.
    pub fn detail(&self, chunk: &str) {
        self.state
            .chars
            .fetch_add(chunk.chars().count(), Ordering::SeqCst);
        let mut ready: Vec<String> = Vec::new();
        {
            let mut buf = self
                .state
                .line_buf
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            buf.push_str(chunk);
            while let Some(nl) = buf.find('\n') {
                let line: String = buf.drain(..=nl).collect();
                ready.push(line);
            }
        }
        for line in ready {
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if trimmed.trim().is_empty() {
                continue;
            }
            self.state.emit_detail_line(trimmed);
        }
        self.state.draw();
    }

    /// Explicit teardown. [`Drop`] does the same; both are idempotent.
    pub fn finish(self) {
        // `Drop` runs `finish` — this exists so call sites can read as a
        // deliberate teardown rather than a `drop()`.
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.state.finish();
    }
}

/// Cover an async operation with a spinner. The handle lives for exactly the
/// future's lifetime, so a **cancelled** future erases the line just as a
/// completed one does — the residue path the old `select!`-on-completion-arm
/// eraser leaked.
pub async fn with_spinner<F: std::future::Future>(
    caps: LineCaps,
    label: &str,
    sink: Sink,
    color: bool,
    fut: F,
) -> F::Output {
    let _spinner = Spinner::start_with_caps(caps, label, sink, color);
    fut.await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pending interrupt swaps the stage label for the acknowledgment; the
    /// caller's own label is untouched and returns when the flag clears.
    #[test]
    fn a_pending_interrupt_overrides_the_stage_label() {
        assert_eq!(effective_label("thinking…", false), "thinking…");
        assert_eq!(effective_label("thinking…", true), INTERRUPT_LABEL);
    }

    /// The process-wide flag round-trips and clears (serial: global state).
    #[serial_test::serial(interrupt_pending)]
    #[test]
    fn interrupt_pending_flag_sets_and_clears() {
        set_interrupt_pending(true);
        assert!(interrupt_pending());
        set_interrupt_pending(false);
        assert!(!interrupt_pending());
    }

    /// The gate is honored end to end: no capability ⇒ no spinner object at
    /// all, so a caller cannot accidentally emit a byte.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn no_capability_yields_no_spinner() {
        assert!(
            Spinner::start_with_caps(LineCaps::None, "thinking…", Sink::Stdout, true).is_none()
        );
    }

    /// §6.9: teardown is unforgettable and happens exactly once, whether it
    /// comes from an explicit `finish()` or from `Drop` on an error/cancel path.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn teardown_is_idempotent_and_unforgettable() {
        let sp = Spinner::start_with_caps(LineCaps::Own, "thinking…", Sink::Stdout, false)
            .expect("Own yields a spinner");
        let state = sp.state.clone();
        assert!(!state.finished.load(Ordering::SeqCst));
        drop(sp);
        assert!(
            state.finished.load(Ordering::SeqCst),
            "Drop must tear the spinner down"
        );
        // A second teardown is a no-op rather than a second erase.
        state.finish();
        assert!(state.finished.load(Ordering::SeqCst));
    }

    /// Dropping the handle releases the LINE too, so the next writer can take
    /// it. (Without this, a leaked ticker reference would pin the lease and the
    /// arbiter would hand out nothing ever again.)
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn dropping_a_spinner_releases_the_line_for_the_next_writer() {
        let sp = Spinner::start_with_caps(LineCaps::Own, "thinking…", Sink::Stdout, false)
            .expect("spinner");
        assert!(
            Spinner::start_with_caps(LineCaps::Own, "other…", Sink::Stdout, false).is_none(),
            "one line, one writer"
        );
        drop(sp);
        assert!(
            Spinner::start_with_caps(LineCaps::Own, "next…", Sink::Stdout, false).is_some(),
            "the line is free again"
        );
    }

    /// **The race this PR closes.** `suspend_for_prompt` sets `suspended` and
    /// then erases; an in-flight `draw` that already passed `paint`'s
    /// `suspended()` check must not be able to flush its frame after that
    /// erase, or the question is painted over and the operator is blocked on
    /// a prompt they cannot see.
    ///
    /// Deterministic by construction: the test holds `paint_gate` itself,
    /// standing in for the draw that is inside it and about to write. The
    /// handshake means the erasing thread is provably parked *at the call*
    /// before the observation window opens — so "it did not proceed" is a
    /// reading about the gate, not about a thread that had not started yet.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn erase_waits_for_an_in_flight_draw_instead_of_racing_it() {
        let sp = Spinner::start_with_caps(LineCaps::Own, "thinking…", Sink::Stdout, false)
            .expect("spinner");
        let state = sp.state.clone();
        // Stand in for a `draw` that holds the gate and has not written yet.
        let in_flight = state
            .paint_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let at_call = Arc::new((Mutex::new(false), Condvar::new()));
        let erased = Arc::new(AtomicBool::new(false));
        let eraser = {
            let (state, at_call, erased) = (state.clone(), at_call.clone(), erased.clone());
            std::thread::spawn(move || {
                {
                    let (m, cv) = &*at_call;
                    *m.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                    cv.notify_all();
                }
                Ephemeral::erase(&*state);
                erased.store(true, Ordering::SeqCst);
            })
        };
        {
            let (m, cv) = &*at_call;
            let mut at = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*at {
                at = cv
                    .wait(at)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !erased.load(Ordering::SeqCst),
            "erase ran while a draw held the gate — that frame's bytes would land AFTER the erase"
        );
        drop(in_flight);
        eraser.join().expect("the erasing thread");
        assert!(
            erased.load(Ordering::SeqCst),
            "erase must proceed once the in-flight draw releases the gate"
        );
    }

    /// The gate is now shared by three paths, so teardown must still compose:
    /// a second erase is a no-op rather than a second escape, and `finish`
    /// after an erase must not deadlock on a gate the erase already released.
    /// (A reentrant implementation — `erase` calling `finish`, or `finish`
    /// routing through `Ephemeral::erase` — hangs here instead of failing.)
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn erase_stays_idempotent_and_composes_with_finish() {
        let sp = Spinner::start_with_caps(LineCaps::Own, "thinking…", Sink::Stdout, false)
            .expect("spinner");
        let state = sp.state.clone();
        Ephemeral::erase(&*state);
        Ephemeral::erase(&*state);
        state.finish();
        Ephemeral::erase(&*state);
        assert!(state.finished.load(Ordering::SeqCst));
    }

    /// The stage label is mutable without resetting the elapsed clock — the
    /// `thinking…` → `compressing context…` transition.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn the_label_changes_without_restarting_the_clock() {
        let sp = Spinner::start_with_caps(LineCaps::Own, "thinking…", Sink::Stdout, false)
            .expect("spinner");
        let started = sp.state.start;
        sp.set_label("compressing context…");
        assert_eq!(
            *sp.state
                .label
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            "compressing context…"
        );
        assert_eq!(sp.state.start, started, "the clock is not reset");
    }

    /// Detail chunks accumulate the `· N chars` counter and only flush COMPLETE
    /// lines to scrollback; a partial line is held until its newline arrives.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn detail_buffers_partial_lines_and_counts_every_char() {
        let sp = Spinner::start_with_caps(LineCaps::Own, "thinking…", Sink::Stdout, false)
            .expect("spinner");
        sp.detail("abc");
        assert_eq!(sp.state.chars.load(Ordering::SeqCst), 3);
        assert_eq!(
            *sp.state
                .line_buf
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            "abc",
            "a line with no newline yet is held, not emitted"
        );
        sp.detail("de\nfg");
        assert_eq!(sp.state.chars.load(Ordering::SeqCst), 8);
        assert_eq!(
            *sp.state
                .line_buf
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            "fg",
            "the completed line flushed; the remainder is still buffered"
        );
    }

    /// A cancelled future erases the line. This is the second residue path the
    /// old implementation leaked: its erase lived on the completion arm of a
    /// `select!`, so a dropped future left a glyph on screen forever.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn a_cancelled_covered_future_still_erases() {
        let fut = with_spinner(
            LineCaps::Own,
            "thinking…",
            Sink::Stdout,
            false,
            std::future::pending::<()>(),
        );
        let mut fut = Box::pin(fut);
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        // Poll once so the spinner is really constructed and holds the line…
        assert!(std::future::Future::poll(fut.as_mut(), &mut cx).is_pending());
        assert!(
            Spinner::start_with_caps(LineCaps::Own, "other…", Sink::Stdout, false).is_none(),
            "the covered future holds the line while it runs"
        );
        // …then CANCEL it rather than completing it.
        drop(fut);
        assert!(
            Spinner::start_with_caps(LineCaps::Own, "after…", Sink::Stdout, false).is_some(),
            "cancelling the covered future must release and erase the line"
        );
    }
}
