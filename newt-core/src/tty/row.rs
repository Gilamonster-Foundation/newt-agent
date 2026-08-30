//! [`EphemeralRow`] — ownership of the terminal's one ephemeral bottom row,
//! extracted from `SpinnerState` (D2b, #1895).
//!
//! # Why this is its own type
//!
//! The spinner used to be two things at once: the thing that *knows what the
//! row should say*, and the thing that *owns the row*. D2's cutover separated
//! them — the renderer ([`super::progress_sink::TerminalProgressSink`]) owns
//! the row, the spinner produces facts — and a `LineLease` is **exclusive**
//! (`Inner::line_held`, with a 50 ms wait-timeout), so the two owners could not
//! briefly coexist while ownership moved. Extracting the mechanics first,
//! unchanged, is what made the handover a single edit rather than a window in
//! which both held or neither did.
//!
//! # The concurrency here is load-bearing and was moved VERBATIM
//!
//! Every rule below was established by an earlier fix and is preserved exactly,
//! not re-derived. Read them as constraints, not preferences:
//!
//! - **Lock order is always `paint_gate` → stdout**, in every holder.
//! - **[`Ephemeral::erase`] must take the gate.** `Terminal::suspend_for_prompt`
//!   sets `suspended` and *then* erases every ephemeral. Without the gate a tick
//!   can pass `LineLease::paint`'s `suspended()` check while the flag is still
//!   clear, lose the CPU, and flush its frame *after* the erase — repainting the
//!   row the question is about to occupy. That is the invisible-prompt hang the
//!   arbiter exists to end, arriving through the one door it did not close.
//! - **`finish` is idempotent**, so `Drop` and an explicit teardown are the same
//!   operation and can never double-erase a row someone else has since taken.
//! - **A paint after `finish` must not land.** A tick could otherwise pass the
//!   finished check, lose the CPU, and paint after the erase — harmless when the
//!   next writer starts a fresh row, destructive when it is a viewport that has
//!   just taken this one (#1727's spinner → live-output hand-off).
//!
//! Both callers of [`Ephemeral::erase`] — `Terminal::suspend_for_prompt` and
//! `Terminal::emit_line` — collect registered ephemerals under the arbiter mutex
//! and **release it before erasing**, so no thread holds the arbiter mutex while
//! waiting on this gate and there is no cycle to deadlock on.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use super::arbiter::{Ephemeral, LineLease, LineWriter, Sink, Terminal};
use super::caps::LineCaps;

/// Exclusive ownership of the ephemeral bottom row.
///
/// Holding one of these IS ownership of that row: there is exactly one, the
/// arbiter enforces it, and everything that draws there goes through this type.
pub(crate) struct EphemeralRow {
    lease: LineLease,
    /// Serializes [`paint`](Self::paint) against [`finish`](Self::finish) and
    /// [`Ephemeral::erase`]. See the module docs — this is the #1727 gate and
    /// the invisible-prompt fix, not defensive locking.
    ///
    /// Visible within `tty` so the race tests that PROVE the gate can stand in
    /// for an in-flight paint by holding it. Those tests are the reason the
    /// gate exists; they must be able to address it.
    pub(super) paint_gate: Mutex<()>,
    finished: AtomicBool,
}

impl EphemeralRow {
    /// Take the row, or `None` when this process may not own one — in which
    /// case **zero bytes are emitted, on any stream**. Protocol mode is an
    /// absolute veto no capability override may pierce.
    ///
    /// Returns an `Arc` and **registers itself** with the arbiter, because a
    /// row that is not registered is not erased when a question is about to
    /// render — the invisible-prompt hang, arriving through a caller that
    /// forgot a second call. Registration is the row's own business, so there
    /// is no way to hold one that skipped it.
    pub(crate) fn acquire(caps: LineCaps, sink: Sink) -> Option<Arc<Self>> {
        let lease = Terminal::lease_with_caps(caps, sink)?;
        let row = Arc::new(Self {
            lease,
            paint_gate: Mutex::new(()),
            finished: AtomicBool::new(false),
        });
        let as_ephemeral: Arc<dyn Ephemeral> = row.clone();
        Terminal::register(
            Arc::as_ptr(&row) as *const () as usize as u64,
            &as_ephemeral,
        );
        Some(row)
    }

    /// Redraw the row in place, unless it has finished.
    ///
    /// The caller composes the content; this owns *when* it may land.
    pub(crate) fn paint(&self, f: impl FnOnce(&mut LineWriter<'_>) -> io::Result<()>) {
        let _gate = self
            .paint_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.finished.load(Ordering::SeqCst) {
            return;
        }
        self.lease.paint(f);
    }

    /// Write a permanent line, below and behind the ephemeral row.
    ///
    /// Deliberately **not** gated: `Terminal::emit_line` erases every
    /// registered ephemeral itself before writing, and taking the gate here
    /// would invert the established `paint_gate` → stdout order.
    pub(crate) fn emit_line(&self, f: impl FnOnce(&mut LineWriter<'_>) -> io::Result<()>) {
        self.lease.emit_line(f);
    }

    /// Erase and stop, exactly once.
    ///
    /// `before_erase` runs **under the gate**, immediately before the erase, so
    /// a caller can flush a trailing line without racing the erase that is
    /// about to wipe the row.
    pub(crate) fn finish(&self, before_erase: impl FnOnce(&Self)) {
        let _gate = self
            .paint_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        before_erase(self);
        self.lease.erase();
    }

    /// Whether teardown has run. Test-only: production code learns it by the
    /// row being taken (see `TerminalProgressSink::finish`).
    #[cfg(test)]
    pub(super) fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }
}

impl Ephemeral for EphemeralRow {
    /// Erase through the SAME gate as [`paint`](Self::paint) and
    /// [`finish`](Self::finish) — see the module docs for why this is the
    /// invisible-prompt fix rather than defensive locking.
    ///
    /// Taking the gate makes exactly two orderings possible:
    ///
    /// - a paint was already inside → this erase waits for it, then clears what
    ///   it wrote (`painted` is set, so the erase is real);
    /// - a paint arrives after → it takes the gate next and finds `suspended()`
    ///   true, so it writes nothing.
    fn erase(&self) {
        let _gate = self
            .paint_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.lease.erase();
    }

    fn restore(&self) {
        // Nothing to do: the shared ticker repaints within one frame, and
        // repainting here would race the question's own final flush.
    }
}
