//! The ONE spinner, driven by the ONE ticker. **A producer, not a writer**
//! (D2b, #1895).
//!
//! # It owns no row
//!
//! This module emits no bytes on any stream. It measures — a stage label, an
//! elapsed clock, a character count, the model's trickle — and publishes those
//! facts to a [`ProgressSink`]. The ephemeral bottom row belongs to the
//! renderer ([`TerminalProgressSink`]), which is the only thing that paints it.
//!
//! That split is forced, not stylistic. The arbiter hands the row to exactly
//! one holder, so a spinner that both held it and published would be two
//! writers on one row — the defect `tty` exists to prevent, and the reason D2a
//! deferred this cutover until the renderer could own the row.
//! `the_spinner_owns_no_ephemeral_row` fails the build if that comes back, and
//! it matches this prose too: naming the row-owning types here, even in a
//! comment, fires the guard. Say it in words, as above.
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

use std::borrow::Cow;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use crate::progress::{Durable, Frame, Note, ProgressSink, TaskId};

use super::arbiter::Sink;
use super::caps::LineCaps;
use super::progress_sink::TerminalProgressSink;
use super::widgets::Level;

/// The unit of work a spinner's events are about.
///
/// A program constant, not operator data — see [`TaskId`]. One id for the
/// family: a spinner covers whatever its label says it covers, and the label
/// travels on the frame.
pub const SPINNER_TASK: TaskId = "spinner";

/// The shared frame cadence. 100 ms unifies the three clocks this replaces
/// (event-driven, `interval(120ms)`, and `poll/sleep(100ms)`); 120 ms was the
/// odd one out, so this is the smallest visual delta available.
const TICK: Duration = Duration::from_millis(100);

/// Process-wide "the user asked to interrupt" signal.
///
/// Bumped by the TUI's keyboard watcher on EVERY interrupt press (Esc or
/// Ctrl-C — the 1st, 2nd and Nth), cleared when the turn ends. The spinner
/// reads it on every frame and swaps its stage label for the acknowledgment
/// ([`interrupt_label`]), so each press is acknowledged on screen within one
/// tick (~100 ms) — through the line the spinner already owns, never a second
/// terminal writer (the #1312 rule). Without this, a graceful cancel is
/// invisible until the turn reaches its next checkpoint and the whole TUI
/// reads as hung.
///
/// A COUNT rather than a flag (#2010): a repeated press used to raise a
/// second flag that nothing acted on until the turn returned, so the 2nd and
/// 10th press were indistinguishable from the 1st — precisely the "will not
/// immediately respond" the operator reported. The first press already drops
/// the in-flight request and any running tool future (`cancellable` in
/// `agentic`), so there is nothing more for a second press to force; what it
/// is owed is an honest, immediate "heard — already stopping".
static INTERRUPT_PRESSES: AtomicU32 = AtomicU32::new(0);

/// The acknowledgment label shown in place of the stage label after the
/// first interrupt press. Later presses append their count — see
/// [`interrupt_label`].
pub const INTERRUPT_LABEL: &str = "interrupting…";

/// The label for `presses` interrupt presses (≥ 1): the plain acknowledgment
/// for the first, and an honest "already stopping" carrying the count for
/// every press after it, so a repeat is visibly heard rather than absorbed.
fn interrupt_label(presses: u32) -> Cow<'static, str> {
    if presses <= 1 {
        Cow::Borrowed(INTERRUPT_LABEL)
    } else {
        Cow::Owned(format!(
            "{INTERRUPT_LABEL} (×{presses} heard — already stopping)"
        ))
    }
}

/// Record one interrupt press and return the running count for this turn.
/// The TUI watchers call it on every Esc/Ctrl-C; the spinner renders the
/// count on its next tick.
pub fn note_interrupt_press() -> u32 {
    INTERRUPT_PRESSES.fetch_add(1, Ordering::SeqCst) + 1
}

/// Flag/clear the pending-interrupt acknowledgment: `true` counts as one
/// press, `false` resets the count. The turn wrapper clears it when the turn
/// hands back.
pub fn set_interrupt_pending(on: bool) {
    INTERRUPT_PRESSES.store(u32::from(on), Ordering::SeqCst);
}

/// Whether an interrupt acknowledgment is currently pending.
pub fn interrupt_pending() -> bool {
    interrupt_presses() > 0
}

/// How many interrupt presses this turn has acknowledged so far.
pub fn interrupt_presses() -> u32 {
    INTERRUPT_PRESSES.load(Ordering::SeqCst)
}

/// The stage label a frame should actually show: the caller's label normally,
/// the interrupt acknowledgment (with its press count) while a cancel is
/// pending. Pure for testing.
fn effective_label(label: &str, presses: u32) -> Cow<'_, str> {
    if presses == 0 {
        Cow::Borrowed(label)
    } else {
        interrupt_label(presses)
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// The dim, indented register the model's reasoning trickle scrolls in.
///
/// The indent lives in the text because [`Notice`](super::widgets::Notice)'s
/// gutter is a *glyph* gutter and a trickle line has no glyph — the same shape
/// the widget already documents for "text that leads with its own marker".
fn detail_note(line: &str) -> Note {
    Note {
        level: Level::Dim,
        glyph: "",
        text: format!("  {line}"),
    }
}

struct SpinnerState {
    start: Instant,
    label: Mutex<String>,
    /// Characters of detail seen so far — drives the `· N chars` tail.
    chars: AtomicUsize,
    /// Partial detail line awaiting its newline.
    line_buf: Mutex<String>,
    /// Where the facts go. The renderer owns the row; this owns none.
    ///
    /// Concrete rather than `Box<dyn ProgressSink>` because teardown hands the
    /// trailing partial line to [`TerminalProgressSink::finish`], which flushes
    /// it under the row's gate — an ordering the renderer-neutral trait has no
    /// vocabulary for and should not grow one for.
    view: Mutex<TerminalProgressSink>,
}

impl SpinnerState {
    /// Publish one frame: the stage label, the elapsed clock and the units
    /// counted so far. **No glyph and no fitted string** — the view composes
    /// those, because only the view knows how wide it is or how fast it spins.
    fn publish_frame(&self) {
        let frame = self.current_frame();
        self.view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .frame(SPINNER_TASK, Self::at_ms(frame.elapsed), &frame);
    }

    /// The frame this spinner would publish right now.
    ///
    /// Separated from the publishing so the *facts* are assertable without a
    /// sink and without a terminal — the same split `Notice::line` /
    /// `Notice::emit` uses, for the same reason.
    fn current_frame(&self) -> Frame {
        let label = self
            .label
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Frame::new(effective_label(&label, interrupt_presses()))
            .after(self.start.elapsed())
            .with_units(self.chars.load(Ordering::SeqCst) as u64)
    }

    /// Publish one completed detail line as durable progress.
    fn publish_detail_line(&self, line: &str) {
        let at = Self::at_ms(self.start.elapsed());
        self.view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(SPINNER_TASK, at, &Durable::Note(detail_note(line)));
    }

    /// The producer's own clock, in the units the sink takes. `progress` reads
    /// no clock; the spinner always did.
    fn at_ms(elapsed: Duration) -> u64 {
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
    }

    fn tick(&self) {
        self.publish_frame();
    }

    /// Stop, flushing a trailing partial detail line so nothing the model said
    /// is lost to the erase.
    ///
    /// Idempotent — `Drop` and an explicit `finish()` are the same operation,
    /// so teardown can never double-erase a row someone else has since taken.
    /// The buffer is drained rather than read, and the renderer's teardown
    /// takes the row, so a second call has nothing left to do.
    fn finish(&self) {
        let tail = {
            let mut buf = self
                .line_buf
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *buf)
        };
        let trimmed = tail.trim_end_matches(['\n', '\r']);
        let trailing = (!trimmed.trim().is_empty()).then(|| detail_note(trimmed));
        self.view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .finish(trailing);
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
        // The RENDERER takes the row. `None` here is the same answer this
        // constructor has always given when the process may not own a line —
        // no spinner object at all, so a caller cannot emit a byte by mistake.
        let view = TerminalProgressSink::animating(caps, sink, color)?;
        let state = Arc::new(SpinnerState {
            start: Instant::now(),
            label: Mutex::new(label.to_string()),
            chars: AtomicUsize::new(0),
            line_buf: Mutex::new(String::new()),
            view: Mutex::new(view),
        });
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
            self.state.publish_detail_line(trimmed);
        }
        self.state.publish_frame();
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
    use crate::progress::Commit;

    /// **D2b's structural single-writer proof.** The spinner must not own the
    /// ephemeral row: the renderer does. One writer on that row is the defect
    /// `tty` exists to prevent, and D2a deferred this cutover precisely
    /// because publishing from inside a lease-holding spinner would have made
    /// two.
    ///
    /// Structural rather than behavioural on purpose. A behavioural test can
    /// only prove that two writers did not collide *on the runs it observed*;
    /// this proves the spinner has no writer to collide with. #1866 is the
    /// cautionary case — a veto that holds by reachability rather than
    /// construction, broken silently by the next caller.
    ///
    /// Needles are assembled with `concat!` so this file cannot match itself,
    /// and the source is embedded at COMPILE time (no filesystem I/O).
    #[test]
    fn the_spinner_owns_no_ephemeral_row() {
        let src = include_str!("spinner.rs");
        let production = src.split(concat!("#[cfg(", "test)]")).next().unwrap_or(src);
        for needle in [
            // Owning the row by any name. `EphemeralRow` is here because the
            // extraction commit made this guard pass while the spinner still
            // OWNED the row — it had merely stopped naming `LineLease`. A
            // deletion gate that goes green before the deletion is the
            // vacuous-green failure this epic keeps finding, so the needle is
            // the ownership, not the spelling.
            concat!("Ephemeral", "Row"),
            concat!("Line", "Lease"),
            concat!("lease", ".paint("),
            concat!("lease", ".erase("),
            concat!("lease", ".emit_line("),
            concat!("impl Ephemeral", " for"),
        ] {
            assert!(
                !production.contains(needle),
                "spinner production code still references `{needle}` — the \
                 renderer owns the ephemeral row after D2b (#1895 constraint 1). \
                 Two writers on one row is the defect `tty` exists to prevent."
            );
        }
    }

    /// What the spinner's renderer has committed so far.
    fn committed_by(state: &SpinnerState) -> Vec<crate::progress::Commit> {
        state
            .view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .committed()
            .to_vec()
    }

    /// The row is free ONLY once nothing holds it: the arbiter is one-lease, so
    /// an animating renderer starting IS the proof.
    fn row_is_free() -> bool {
        super::TerminalProgressSink::animating(LineCaps::Own, Sink::Stdout, false).is_some()
    }

    /// A pending interrupt swaps the stage label for the acknowledgment; the
    /// caller's own label is untouched and returns when the flag clears.
    #[test]
    fn a_pending_interrupt_overrides_the_stage_label() {
        assert_eq!(effective_label("thinking…", 0), "thinking…");
        assert_eq!(effective_label("thinking…", 1), INTERRUPT_LABEL);
    }

    /// #2010: a repeated press is visibly different from the first — the
    /// label carries the count and says the turn is already stopping, so the
    /// operator can tell a slow cancel from a dropped keystroke.
    #[test]
    fn a_repeated_press_is_labelled_with_its_count() {
        assert_eq!(
            effective_label("thinking…", 2),
            "interrupting… (×2 heard — already stopping)"
        );
        assert_eq!(
            effective_label("thinking…", 7),
            "interrupting… (×7 heard — already stopping)"
        );
    }

    /// The process-wide count round-trips and clears (serial: global state).
    #[serial_test::serial(interrupt_pending)]
    #[test]
    fn interrupt_pending_flag_sets_and_clears() {
        set_interrupt_pending(true);
        assert!(interrupt_pending());
        assert_eq!(interrupt_presses(), 1);
        set_interrupt_pending(false);
        assert!(!interrupt_pending());
        assert_eq!(interrupt_presses(), 0);
    }

    /// Every press counts, from a clean turn: 1, 2, 3 — and a clear resets.
    #[serial_test::serial(interrupt_pending)]
    #[test]
    fn every_interrupt_press_is_counted() {
        set_interrupt_pending(false);
        assert_eq!(note_interrupt_press(), 1);
        assert_eq!(note_interrupt_press(), 2);
        assert_eq!(note_interrupt_press(), 3);
        assert_eq!(interrupt_presses(), 3);
        set_interrupt_pending(false);
        assert_eq!(interrupt_presses(), 0);
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
    ///
    /// The row itself is the renderer's now, so what the spinner can be held to
    /// is that it hands teardown on — and that the row it was using comes free.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn teardown_is_idempotent_and_unforgettable() {
        let sp = Spinner::start_with_caps(LineCaps::Own, "thinking…", Sink::Stdout, false)
            .expect("Own yields a spinner");
        let state = sp.state.clone();
        drop(sp);
        assert!(row_is_free(), "Drop must tear the spinner down");
        // A second teardown is a no-op rather than a second erase — and, since
        // the row is now free and may belong to someone else, that matters more
        // than it did when the spinner owned it (#1727).
        state.finish();
        assert!(row_is_free());
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

    /// A completed detail line commits as a dim, indented note — **byte for
    /// byte the line the spinner used to write itself**, now composed by the
    /// renderer from a `Durable` that says what it means.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn a_completed_detail_line_commits_as_an_indented_dim_note() {
        let sp = Spinner::start_with_caps(LineCaps::Own, "thinking…", Sink::Stdout, false)
            .expect("spinner");
        sp.detail("hello\nwor");
        let committed = committed_by(&sp.state);
        assert_eq!(
            committed,
            vec![Commit::Note(Note {
                level: Level::Dim,
                glyph: "",
                text: "  hello".into(),
            })],
            "only the COMPLETE line commits; the partial one is still buffered"
        );
        assert_eq!(
            super::TerminalProgressSink::line_of(&committed[0]),
            "  hello",
            "the two-space dim indent survives the cutover"
        );
    }

    /// The trailing partial line is flushed at teardown rather than lost to the
    /// erase — and flushed exactly once, because teardown drains the buffer.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn the_trailing_partial_line_is_flushed_at_teardown_exactly_once() {
        let sp = Spinner::start_with_caps(LineCaps::Own, "thinking…", Sink::Stdout, false)
            .expect("spinner");
        sp.detail("no newline yet");
        assert!(
            committed_by(&sp.state).is_empty(),
            "a partial line is held, not committed"
        );
        let state = sp.state.clone();
        drop(sp);
        assert_eq!(
            committed_by(&state),
            vec![Commit::Note(Note {
                level: Level::Dim,
                glyph: "",
                text: "  no newline yet".into(),
            })],
            "nothing the model said is lost to the erase"
        );
        state.finish();
        assert_eq!(
            committed_by(&state).len(),
            1,
            "teardown is idempotent — the buffer is drained, not read"
        );
    }

    /// **Constraint 3, with a LIVE producer.** `progress`'s
    /// `a_high_rate_producer_grows_neither_sink` models this traffic
    /// synthetically — 10k frames plus 10k non-advancing snapshots; this is the
    /// real spinner behind the frame half of it, painting a real row.
    ///
    /// 1k frames is a ~100-second turn at the shared 100 ms cadence. The count
    /// is lower than its synthetic twin's on purpose: every paint here costs a
    /// terminal width probe, and retention is O(1) *by construction* — the
    /// renderer keeps no per-frame state and there is no `Frame` → `Commit` —
    /// so a longer loop buys confidence in nothing but the clock.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn a_live_spinner_at_frame_rate_commits_nothing_and_retains_nothing() {
        let sp = Spinner::start_with_caps(LineCaps::Own, "thinking…", Sink::Stdout, false)
            .expect("spinner");
        for _ in 0..1_000 {
            sp.state.tick();
        }
        assert!(
            committed_by(&sp.state).is_empty(),
            "1k frames committed: {:?}",
            committed_by(&sp.state)
        );
    }

    /// The published frame carries **measurable facts** — the stage label the
    /// caller set, and the units counted so far. No glyph, no fitted string:
    /// those are the view's, and `a_frame_carries_no_rendering_and_no_glyph`
    /// fails the build if the type grows them back.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn the_published_frame_carries_the_stage_label_and_the_units() {
        let sp = Spinner::start_with_caps(LineCaps::Own, "thinking…", Sink::Stdout, false)
            .expect("spinner");
        sp.detail("abcd");
        let frame = sp.state.current_frame();
        assert_eq!(frame.label, "thinking…");
        assert_eq!(
            frame.units, 4,
            "the `· N chars` tail is a fact, not a string"
        );
        sp.set_label("compressing context…");
        assert_eq!(sp.state.current_frame().label, "compressing context…");
    }

    /// A pending interrupt reaches the operator through the FRAME, within one
    /// tick: the spinner substitutes the acknowledgment when it publishes, so
    /// the press is on screen ~100 ms later through the row the renderer
    /// already owns — never a second terminal writer (the #1312 rule).
    #[serial_test::serial(tty_arbiter)]
    #[serial_test::serial(interrupt_pending)]
    #[test]
    fn a_pending_interrupt_reaches_the_view_through_the_frame() {
        let sp = Spinner::start_with_caps(LineCaps::Own, "thinking…", Sink::Stdout, false)
            .expect("spinner");
        set_interrupt_pending(false);
        note_interrupt_press();
        assert_eq!(sp.state.current_frame().label, INTERRUPT_LABEL);
        // #2010: the second press changes the frame too — it is not absorbed.
        note_interrupt_press();
        assert_eq!(
            sp.state.current_frame().label,
            "interrupting… (×2 heard — already stopping)"
        );
        set_interrupt_pending(false);
        assert_eq!(sp.state.current_frame().label, "thinking…");
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
