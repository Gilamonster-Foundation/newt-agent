//! The terminal renderer for [`crate::progress`] — wired to the **spinner
//! family only** (D2a #1864, cut over in D2b #1895).
//!
//! # One family at a time
//!
//! D2a dual-publishes: the new contract runs beside the existing
//! stdout/`TurnDriver` paths, and *those stay authoritative*. A renderer that
//! emitted for every family at once would print each notice twice — once from
//! the old path, once from this one — which is not dual-publishing, it is a
//! regression wearing the word. So each family switches onto this renderer in
//! its own **cutover** PR, with its own deletion gate, and every family that
//! has not been cut over keeps its old path.
//!
//! D2b cut over the **spinner**: `tty::spinner` now owns no row and paints
//! nothing, and this renderer owns the ephemeral bottom row and animates it.
//! `no_production_path_renders_progress_yet` still holds the line for the
//! three families that have NOT been cut over.
//!
//! # The renderer owns the row, and that ordering was forced
//!
//! A [`LineLease`](crate::tty::LineLease) is exclusive with a 50 ms
//! wait-timeout, so a renderer acquiring the row while the spinner still held
//! one would not briefly coexist — it would time out and get nothing. There is
//! no overlap window, which is why row ownership moved as a **type**
//! ([`EphemeralRow`], extracted first and unchanged) rather than being
//! negotiated between two live objects. Publishing from inside a lease-holding
//! spinner would have put two writers on one row, which is the defect `tty`
//! exists to prevent.
//!
//! # The view supplies the glyph
//!
//! A [`Frame`] carries measurable facts and no rendering: no fitted string, no
//! glyph index. The animation is therefore **this** module's, derived from the
//! one fact the frame does carry — [`Frame::elapsed`] — at a cadence
//! ([`GLYPH_PERIOD`]) the view picks. That keeps the producer's tick rate out
//! of the animation: a burst of detail chunks refreshes the row without
//! spinning the glyph faster, and a view that would rather draw a bar or a
//! percentage ignores the cycle entirely.
//!
//! # The veto, and why it is a pure predicate
//!
//! [`may_render`] takes `protocol` as a *parameter* rather than reading
//! [`protocol_mode`]. That is deliberate, and it is not merely for tidiness:
//! `enter_protocol_mode()` is documented irreversible — "there is no leaving
//! it" — so a test that entered protocol mode to prove the veto would poison
//! every later test in the same binary, including the anti-vacuous twin that
//! has to show rendering happens when the veto is absent. `caps::probe`
//! already established this shape for exactly this reason; this mirrors it.
//!
//! The predicate is the same one [`Notice::emit`] applies, promoted rather
//! than re-derived: protocol mode is an absolute veto, and a process that may
//! not own a terminal line does not narrate into someone's captured log.

use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use crossterm::queue;
use crossterm::style::{Color as CtColor, Print, ResetColor, SetForegroundColor};

use crate::progress::{Commit, Durable, Frame, Note, ProgressSink, Scrollback, TaskId};
use crate::tty::caps::{protocol_mode, LineCaps};
use crate::tty::row::EphemeralRow;
use crate::tty::widgets::{Level, Notice};
use crate::tty::{fit_line, format_spinner, term_cols, Sink, FADE_CT, SPINNER_FRAMES};

/// How long the view shows each spinner glyph.
///
/// **The view's cadence, not the producer's.** It matches the shared ticker's
/// 100 ms so the visible animation is unchanged, but nothing couples them: the
/// glyph is a function of [`Frame::elapsed`], so publishing more often paints
/// a fresher row without spinning faster, and publishing less often slows the
/// clock without stalling the glyph mid-cycle.
const GLYPH_PERIOD: Duration = Duration::from_millis(100);

/// May a progress renderer put bytes on the terminal?
///
/// Pure, so the whole cartesian product is table-testable without a terminal
/// and without tripping the irreversible global. See the module docs.
#[must_use]
pub(crate) fn may_render(caps: LineCaps, protocol: bool) -> bool {
    // A protocol channel on fd 1 vetoes everything, on every platform — an
    // explicitly-supplied `Own` cannot pierce it, because fd 1 may be a
    // JSON-RPC wire.
    if protocol {
        return false;
    }
    caps.can_own()
}

/// Renders committed progress as permanent lines through the ONE [`Notice`]
/// seam, and — when it holds the row — animates transient frames on it.
///
/// The two constructors are two different things, not a flag:
/// [`new`](Self::new) is commit-only and infallible; [`animating`](Self::animating)
/// takes the ONE ephemeral bottom row and is therefore fallible, because a
/// process that may not own a line, or one where somebody else already holds
/// it, must get **no** animator rather than a silent second writer.
pub struct TerminalProgressSink {
    caps: LineCaps,
    sink: Sink,
    color: bool,
    scrollback: Scrollback,
    /// The ephemeral bottom row, when this renderer took it. `None` is a
    /// commit-only renderer: frames are dropped, there being no row to paint.
    row: Option<Arc<EphemeralRow>>,
}

impl std::fmt::Debug for TerminalProgressSink {
    /// Hand-written because [`EphemeralRow`] is a terminal resource, not a
    /// value: what a reader needs is whether this renderer holds the row.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalProgressSink")
            .field("caps", &self.caps)
            .field("sink", &self.sink)
            .field("color", &self.color)
            .field("scrollback", &self.scrollback)
            .field("owns_row", &self.row.is_some())
            .finish()
    }
}

impl TerminalProgressSink {
    /// A **commit-only** renderer that writes to `sink` when `caps` permits.
    ///
    /// Owns no row, so frames are dropped. `sink` is explicit and defaulted
    /// nowhere: relocating a stderr notice to stdout would break someone's
    /// `2>/dev/null`.
    #[must_use]
    pub fn new(caps: LineCaps, sink: Sink, color: bool) -> Self {
        Self {
            caps,
            sink,
            color,
            scrollback: Scrollback::new(),
            row: None,
        }
    }

    /// A renderer that also **owns the ephemeral bottom row** and animates
    /// frames on it, or `None` when this process may not own a line — in which
    /// case **zero bytes are emitted, on any stream**.
    ///
    /// `None` is the same answer `Spinner::start` has always given, for the
    /// same reasons: protocol mode is an absolute veto (fd 1 may be a JSON-RPC
    /// wire), a pipe cannot own a row, and the arbiter hands the row to exactly
    /// one holder.
    #[must_use]
    pub fn animating(caps: LineCaps, sink: Sink, color: bool) -> Option<Self> {
        let row = EphemeralRow::acquire(caps, sink)?;
        let mut me = Self::new(caps, sink, color);
        me.row = Some(row);
        Some(me)
    }

    /// Tear the row down, flushing one last permanent line **under the row's
    /// gate**, immediately before the erase.
    ///
    /// That ordering is the trailing-detail rule moved verbatim rather than
    /// re-derived: a producer's last partial line must land between "mark
    /// finished" and the erase, so no tick can paint over it and the erase
    /// cannot wipe it. The bytes go through the row's own lease rather than
    /// [`Notice::emit`] — the latter routes through
    /// [`Terminal::emit_line`](crate::tty::Terminal::emit_line), which erases
    /// every registered ephemeral through the very gate this call is holding,
    /// and a `std::sync::Mutex` is not reentrant.
    ///
    /// Idempotent: the row is taken, so a second call — including the one
    /// [`Drop`] makes — has nothing to tear down.
    pub fn finish(&mut self, trailing: Option<Note>) {
        let Some(row) = self.row.take() else {
            return;
        };
        // The trailing line goes through the SAME commit rule as every other
        // durable event, so "what scrolled" and "what this recorded" cannot
        // drift — a last line that reached the operator but not `committed()`
        // would make this renderer's own record a lie about its own output.
        let commit = trailing.and_then(|note| self.scrollback.offer(&Durable::Note(note)));
        let allowed = may_render(self.caps, protocol_mode());
        let color = self.color;
        row.finish(|row| {
            if let (Some(commit), true) = (commit, allowed) {
                row.emit_line(Self::notice_of(&commit).writer(color));
            }
        });
    }

    /// The glyph this elapsed time shows — the view's own cycle.
    ///
    /// A [`Frame`] carries no glyph index on purpose, so the cycle lives here.
    /// Deriving it from `elapsed` rather than counting paints is what keeps the
    /// producer's cadence out of the animation.
    #[must_use]
    fn glyph_index(elapsed: Duration) -> usize {
        let periods = elapsed.as_millis() / GLYPH_PERIOD.as_millis();
        (periods % SPINNER_FRAMES.len() as u128) as usize
    }

    /// The line a frame renders as, unfitted — pure, no ANSI, no I/O.
    ///
    /// Split out for the same reason [`Notice::line`] is: the text contract is
    /// testable off the terminal and without a clock, and the painting half
    /// stays a thin adapter.
    #[must_use]
    pub fn frame_line(frame: &Frame) -> String {
        format_spinner(
            Self::glyph_index(frame.elapsed),
            frame.elapsed.as_secs_f32(),
            &frame.label,
            usize::try_from(frame.units).unwrap_or(usize::MAX),
        )
    }

    /// What has committed so far.
    #[must_use]
    pub fn committed(&self) -> &[Commit] {
        self.scrollback.committed()
    }

    /// The row this renderer holds, for the tests that PROVE the gate — they
    /// stand in for an in-flight paint by holding `paint_gate` themselves, and
    /// they are the reason the gate exists, so they must be able to address it.
    #[cfg(test)]
    pub(super) fn row(&self) -> Option<&Arc<EphemeralRow>> {
        self.row.as_ref()
    }

    /// The line a commit renders as — pure, no ANSI, no I/O.
    ///
    /// Split out for the same reason [`Notice::line`] is: the text contract is
    /// testable off the terminal, and the emitting half stays a thin adapter.
    #[must_use]
    pub fn line_of(commit: &Commit) -> String {
        Self::notice_of(commit).line()
    }

    /// The [`Notice`] a commit becomes. One mapping, so the semantics table in
    /// the module docs of [`crate::progress`] has exactly one implementation.
    fn notice_of(commit: &Commit) -> Notice<'_> {
        match commit {
            Commit::Started { label } => Notice::new(Level::Info, "▸", label.clone()).gap(2),
            Commit::Advanced(m) => {
                let text = match m.total {
                    Some(total) => format!("{}/{total}", m.done),
                    None => m.done.to_string(),
                };
                Notice::new(Level::Dim, "·", text).gap(2)
            }
            Commit::Note(n) => Notice::new(n.level, n.glyph, n.text.clone()).gap(2),
            Commit::Finished(outcome) => {
                let (level, glyph, text) = match outcome {
                    crate::progress::Outcome::Completed => (Level::Ok, "✓", "done"),
                    crate::progress::Outcome::Failed => (Level::Warn, "⚠", "failed"),
                    crate::progress::Outcome::Cancelled => (Level::Dim, "⧉", "cancelled"),
                };
                Notice::new(level, glyph, text).gap(2)
            }
        }
    }
}

impl ProgressSink for TerminalProgressSink {
    /// Paint the frame on the row this renderer owns; drop it when it owns
    /// none.
    ///
    /// Always width-fitted: the row is redrawn in place and never scrolls, so a
    /// line wider than the terminal would wrap and strand rows behind it that a
    /// single-line erase can never reach. The width is read per paint, so a
    /// terminal resized mid-turn re-fits — which is exactly why a frame carries
    /// no pre-fitted string.
    ///
    /// Painting is not committing, and cannot become it: there is no code path
    /// here that *could* commit a frame, because [`Frame`] has no conversion to
    /// [`Commit`].
    fn frame(&mut self, _task: TaskId, _at_ms: u64, frame: &Frame) {
        let Some(row) = self.row.as_ref() else {
            return;
        };
        let fitted = fit_line(&Self::frame_line(frame), term_cols());
        let color = self.color;
        row.paint(move |w| {
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

    fn record(&mut self, _task: TaskId, _at_ms: u64, event: &Durable) {
        // The commit rule decides first, so a non-advancing snapshot cannot
        // reach the terminal even where the veto would have allowed bytes.
        let Some(commit) = self.scrollback.offer(event) else {
            return;
        };
        if !may_render(self.caps, protocol_mode()) {
            return;
        }
        Self::notice_of(&commit).emit(self.caps, self.sink, self.color);
    }
}

impl Drop for TerminalProgressSink {
    /// Teardown is unforgettable: whatever path drops this renderer — an early
    /// return, a cancelled future, a panic — the row is erased and released.
    fn drop(&mut self) {
        self.finish(None);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Condvar, Mutex};

    use super::*;
    use crate::progress::{Measure, Outcome};
    use crate::tty::arbiter::Ephemeral;

    const TASK: TaskId = "compaction";

    /// An animating renderer over the real arbiter. Every caller of this is on
    /// the `tty_arbiter` serial lane — there is one ephemeral row per process.
    fn animating() -> TerminalProgressSink {
        TerminalProgressSink::animating(LineCaps::Own, Sink::Stdout, false)
            .expect("Own yields the row")
    }

    fn frame_of(label: &str, ms: u64, units: u64) -> Frame {
        Frame::new(label)
            .after(Duration::from_millis(ms))
            .with_units(units)
    }

    // -----------------------------------------------------------------
    // The view supplies the glyph (D2b, #1895)
    // -----------------------------------------------------------------

    /// The renderer composes the row from a frame's FACTS. The glyph is the
    /// view's, chosen from `elapsed`; the clock and the `· N chars` tail are
    /// the frame's. Byte-identical to what the spinner used to compose inline.
    #[test]
    fn the_view_composes_the_row_from_a_frames_facts() {
        // 1230 ms ⇒ 12 whole glyph periods ⇒ frame 2 of the braille run.
        assert_eq!(
            TerminalProgressSink::frame_line(&frame_of("thinking…", 1230, 340)),
            "⠹ thinking… 1.2s · 340 chars"
        );
        // No units yet ⇒ no tail, exactly as `format_spinner` has always done.
        assert_eq!(
            TerminalProgressSink::frame_line(&frame_of("compressing context…", 500, 0)),
            "⠴ compressing context… 0.5s"
        );
    }

    /// **The glyph follows elapsed time, not paint count.** Painting the same
    /// frame twice must render the same row — a hidden per-paint counter would
    /// make a burst of detail chunks spin the animation faster, which is the
    /// producer's cadence leaking into the view.
    #[test]
    fn the_same_frame_renders_the_same_row_however_often_it_is_painted() {
        let f = frame_of("thinking…", 1230, 340);
        let first = TerminalProgressSink::frame_line(&f);
        for _ in 0..50 {
            assert_eq!(TerminalProgressSink::frame_line(&f), first);
        }
    }

    /// The anti-vacuous twin: the cycle actually moves, and covers the whole
    /// braille run over one period. Without this, `glyph_index` could return a
    /// constant and both tests above would still pass.
    #[test]
    fn and_the_glyph_cycle_covers_the_whole_frame_set() {
        let seen: Vec<usize> = (0..SPINNER_FRAMES.len() as u64)
            .map(|i| TerminalProgressSink::glyph_index(Duration::from_millis(i * 100)))
            .collect();
        assert_eq!(
            seen,
            (0..SPINNER_FRAMES.len()).collect::<Vec<_>>(),
            "one glyph per 100 ms of elapsed time, in order"
        );
        // …and it wraps rather than running off the end of the frame set.
        assert_eq!(
            TerminalProgressSink::glyph_index(Duration::from_millis(1000)),
            0
        );
        assert_eq!(
            TerminalProgressSink::glyph_index(Duration::from_secs(86_400)),
            0,
            "a day-long turn indexes a real glyph, not a panic"
        );
    }

    /// A commit-only renderer holds no row, so a frame has nowhere to land —
    /// and still cannot commit. This is the D2a shape, unchanged: only the
    /// spinner family was cut over.
    #[test]
    fn a_commit_only_renderer_owns_no_row_and_drops_frames() {
        let mut s = TerminalProgressSink::new(LineCaps::Own, Sink::Stderr, false);
        assert!(s.row.is_none(), "`new` is commit-only");
        for t in 0..100 {
            s.frame(TASK, t, &frame_of("f", t, t));
        }
        assert!(s.committed().is_empty());
    }

    // -----------------------------------------------------------------
    // Row ownership: the gate, moved with the row
    // -----------------------------------------------------------------

    /// One row, one owner. The arbiter is one-lease, so a second animating
    /// renderer getting `None` IS the single-writer proof — and the row coming
    /// free on drop is what keeps that from being a one-shot resource.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn only_one_renderer_may_own_the_row_at_a_time() {
        let first = animating();
        assert!(
            TerminalProgressSink::animating(LineCaps::Own, Sink::Stdout, false).is_none(),
            "one row, one writer"
        );
        drop(first);
        assert!(
            TerminalProgressSink::animating(LineCaps::Own, Sink::Stdout, false).is_some(),
            "the row is free again"
        );
    }

    /// No capability ⇒ no row and no renderer that could animate, so a caller
    /// cannot emit a byte by mistake.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn no_capability_yields_no_animating_renderer() {
        assert!(TerminalProgressSink::animating(LineCaps::None, Sink::Stdout, true).is_none());
    }

    /// **The race the arbiter closes**, now addressed where the row lives.
    /// `suspend_for_prompt` sets `suspended` and then erases; an in-flight
    /// paint that already passed `LineLease::paint`'s `suspended()` check must
    /// not be able to flush its frame after that erase, or the question is
    /// painted over and the operator is blocked on a prompt they cannot see.
    ///
    /// Deterministic by construction: the test holds `paint_gate` itself,
    /// standing in for the paint that is inside it and about to write. The
    /// handshake means the erasing thread is provably parked *at the call*
    /// before the observation window opens.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn erase_waits_for_an_in_flight_paint_instead_of_racing_it() {
        let view = animating();
        let row = view.row().expect("the renderer holds the row").clone();
        // Stand in for a paint that holds the gate and has not written yet.
        let in_flight = row
            .paint_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let at_call = Arc::new((Mutex::new(false), Condvar::new()));
        let erased = Arc::new(AtomicBool::new(false));
        let eraser = {
            let (row, at_call, erased) = (row.clone(), at_call.clone(), erased.clone());
            std::thread::spawn(move || {
                {
                    let (m, cv) = &*at_call;
                    *m.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                    cv.notify_all();
                }
                Ephemeral::erase(&*row);
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
            "erase ran while a paint held the gate — that frame's bytes would land AFTER the erase"
        );
        drop(in_flight);
        eraser.join().expect("the erasing thread");
        assert!(
            erased.load(Ordering::SeqCst),
            "erase must proceed once the in-flight paint releases the gate"
        );
    }

    /// The gate is shared by three paths, so teardown must still compose: a
    /// second erase is a no-op rather than a second escape, and `finish` after
    /// an erase must not deadlock on a gate the erase already released. A
    /// reentrant implementation — including a trailing flush routed through
    /// `Notice::emit`, which erases every registered ephemeral through this
    /// very gate — hangs here instead of failing.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn erase_stays_idempotent_and_composes_with_finish() {
        let mut view = animating();
        let row = view.row().expect("the row").clone();
        Ephemeral::erase(&*row);
        Ephemeral::erase(&*row);
        view.finish(Some(Note {
            level: Level::Dim,
            glyph: "",
            text: "  trailing".into(),
        }));
        Ephemeral::erase(&*row);
        assert!(row.is_finished());
    }

    /// Teardown is unforgettable and happens exactly once, whether it comes
    /// from an explicit `finish()` or from `Drop` on an error/cancel path.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn teardown_is_idempotent_and_unforgettable() {
        let mut view = animating();
        let row = view.row().expect("the row").clone();
        assert!(!row.is_finished());
        view.finish(None);
        assert!(row.is_finished());
        // A second teardown is a no-op rather than a second erase, and `Drop`
        // makes one of those on every path.
        view.finish(None);
        assert!(row.is_finished());
        drop(view);
        assert!(row.is_finished());
    }

    /// A frame published after teardown must not land. A tick could otherwise
    /// pass the finished check, lose the CPU, and paint after the erase —
    /// destructive when the next writer is a viewport that has just taken this
    /// row (#1727's spinner → live-output hand-off).
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn a_frame_published_after_teardown_does_not_land() {
        let mut view = animating();
        view.finish(None);
        assert!(view.row.is_none(), "teardown takes the row");
        view.frame(TASK, 0, &frame_of("late…", 100, 0));
        assert!(view.committed().is_empty());
    }

    /// **The veto**, over the whole product. Protocol mode refuses even a
    /// perfect terminal — that is the Windows JSON-RPC protection, where the
    /// fd-redirect is absent and this flag is the only thing between a
    /// progress line and a corrupted wire.
    #[test]
    fn protocol_mode_vetoes_rendering_from_every_capability() {
        let cases: &[(LineCaps, bool, bool)] = &[
            // (caps, protocol, may_render)
            (LineCaps::Own, false, true),
            (LineCaps::Own, true, false),
            (LineCaps::None, false, false),
            (LineCaps::None, true, false),
        ];
        for &(caps, protocol, want) in cases {
            assert_eq!(
                may_render(caps, protocol),
                want,
                "caps={caps:?} protocol={protocol}"
            );
        }
    }

    /// The anti-vacuous twin for the veto: without it, the predicate could be
    /// `false` always and the test above would still pass. Exactly one shape
    /// renders, and this names it.
    #[test]
    fn and_a_real_terminal_outside_protocol_mode_does_render() {
        assert!(
            may_render(LineCaps::Own, false),
            "a real terminal with no protocol guard is the ONE shape that renders — \
             if this fails the veto test above is vacuous"
        );
    }

    /// A sink that cannot own the line emits nothing, and — the part worth
    /// pinning — still *records*, so the contract is exercised identically on
    /// a headless tier. Observation does not depend on rendering.
    #[test]
    fn a_vetoed_sink_still_records_what_would_have_committed() {
        let mut s = TerminalProgressSink::new(LineCaps::None, Sink::Stderr, false);
        s.record(
            TASK,
            0,
            &Durable::Started {
                label: "compacting".into(),
            },
        );
        s.record(TASK, 1, &Durable::Finished(Outcome::Completed));
        assert_eq!(
            s.committed().len(),
            2,
            "the commit rule runs regardless of whether bytes are allowed"
        );
    }

    /// **Animating is not committing.** The renderer that owns the row paints
    /// every frame it is given and commits none of them — the scrollback
    /// boundary holds on the one renderer that now has somewhere to draw. It
    /// cannot be otherwise: there is no `Frame` → `Commit` anywhere, which is
    /// what `nothing_may_add_a_conversion_from_a_frame_to_a_commit` fails the
    /// build over.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn an_animating_renderer_paints_every_frame_and_commits_none() {
        let mut s = animating();
        for t in 0..100 {
            s.frame(TASK, t, &frame_of("thinking…", t, t));
        }
        assert!(
            s.committed().is_empty(),
            "frames committed: {:?}",
            s.committed()
        );
    }

    /// The rendered text is plain — no ANSI in the line contract. `newt-cli`'s
    /// `stdout_purity` asserts zero `\u{1b}` on a protocol wire and every
    /// widget is upstream of it.
    #[test]
    fn rendered_lines_carry_no_ansi_and_no_newline() {
        let commits = [
            Commit::Started {
                label: "compacting".into(),
            },
            Commit::Advanced(Measure::out_of(3, 10)),
            Commit::Advanced(Measure::of(7)),
            Commit::Note(Note {
                level: Level::Warn,
                glyph: "⚠",
                text: "trimmed".into(),
            }),
            Commit::Finished(Outcome::Completed),
            Commit::Finished(Outcome::Failed),
            Commit::Finished(Outcome::Cancelled),
        ];
        for c in &commits {
            let line = TerminalProgressSink::line_of(c);
            assert!(!line.contains('\u{1b}'), "ANSI in {line:?}");
            assert!(!line.contains('\n'), "newline in {line:?}");
            assert!(!line.is_empty(), "empty line for {c:?}");
        }
    }

    /// A measured advance renders its total when it has one, and does not
    /// invent one when it does not.
    #[test]
    fn an_advance_renders_its_total_only_when_it_has_one() {
        assert_eq!(
            TerminalProgressSink::line_of(&Commit::Advanced(Measure::out_of(3, 10))),
            "·  3/10"
        );
        assert_eq!(
            TerminalProgressSink::line_of(&Commit::Advanced(Measure::of(3))),
            "·  3"
        );
    }

    /// **No silent cutover** (constraint 5), for the families that have NOT
    /// been cut over.
    ///
    /// D2b cut the **spinner** family over, which is why `tty/spinner.rs` is no
    /// longer on this list — a named PR with its own deletion gate
    /// (`the_spinner_owns_no_ephemeral_row`), not a silent one. The three
    /// families still on the old path stay on it until their own PRs.
    ///
    /// Both constructors are needles. Listing only `new(` would let the next
    /// cutover slip past by reaching for `animating(` instead, which is the
    /// vacuous-green shape this epic keeps finding.
    ///
    /// Scans sources at COMPILE time, so the guard does no filesystem I/O; the
    /// needles are assembled with `concat!` so this file's own source cannot
    /// match them.
    #[test]
    fn no_production_path_renders_progress_yet() {
        const NEEDLES: &[&str] = &[
            concat!("TerminalProgress", "Sink::new("),
            concat!("TerminalProgress", "Sink::animating("),
        ];
        for (name, src) in [
            (
                "newt-core/src/agentic/driver.rs",
                include_str!("../agentic/driver.rs"),
            ),
            (
                "newt-core/src/agentic/display.rs",
                include_str!("../agentic/display.rs"),
            ),
            (
                "newt-core/src/agentic/note_sink.rs",
                include_str!("../agentic/note_sink.rs"),
            ),
        ] {
            for needle in NEEDLES {
                assert!(
                    !src.contains(needle),
                    "{name} constructs the terminal progress renderer via `{needle}` — \
                     that family has NOT been cut over. Each cutover is its own named PR \
                     with its own deletion gate (#1864 constraint 5)."
                );
            }
        }
    }

    /// The twin: the guard above is looking at real sources. Without this it
    /// would pass just as happily against three empty strings — and it is the
    /// only thing standing between D2a's dual-publish and a silent cutover.
    #[test]
    fn and_the_cutover_scan_is_looking_at_real_sources() {
        for (name, src) in [
            (
                "newt-core/src/agentic/driver.rs",
                include_str!("../agentic/driver.rs"),
            ),
            (
                "newt-core/src/agentic/display.rs",
                include_str!("../agentic/display.rs"),
            ),
            (
                "newt-core/src/agentic/note_sink.rs",
                include_str!("../agentic/note_sink.rs"),
            ),
        ] {
            assert!(
                src.len() > 1000,
                "{name} scanned as {} bytes — the cutover guard would be vacuous",
                src.len()
            );
        }
    }
}
