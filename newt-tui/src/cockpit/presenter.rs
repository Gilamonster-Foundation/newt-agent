//! The presenter: the one writer to the real terminal, and the one reader of
//! the keyboard, for the whole session.
//!
//! # Geometry
//!
//! The screen is `rows` high. The bottom `block_h` rows are the cockpit's:
//! an optional status row (the session's in-progress line — the spinner —
//! plus a `queued` chip), then the ratatui viewport holding the header,
//! palette, editor and tab bar. Everything above `top` is transcript, and it
//! is real scrollback: rows are written with `\r\n` from a known position, so
//! when they push past the bottom the terminal scrolls them into history
//! exactly as a plain `println!` would have.
//!
//! **One cursor query at start, one per resize.** Once fd 1 is on the pty, a plain
//! `ESC[6n` would go to the pty and its answer would never come — so the viewport is
//! [`Viewport::Fixed`], placed by arithmetic, and the position query happens in
//! [`Presenter::open`] before the capture is installed. The only later query is the one a
//! terminal resize needs (#3014): terminals disagree on what a grow does to the screen, so the
//! block's new top is MEASURED, by asking the real terminal where its cursor is
//! ([`PtyCapture::query_cursor`] writes the query to the saved terminal handle, not fd 1) and
//! subtracting where the caret sits in the block. No answer means the old guess
//! ([`resize_new_top`]) and no second question. (A grow pins the block to the new bottom either
//! way; the measurement then tells the resize where the OLD block was, so it can erase it.) Two consequences follow and are enforced
//! here rather than remembered:
//! [`ScrollbackSink`] is implemented by hand (ratatui's `insert_before` is a
//! silent no-op on `Fixed`), and autowrap is OFF while the cockpit owns the
//! terminal with every row pre-wrapped to `cols` — so a row is a row, and the
//! block can never be repainted over a wrapped tail.
//!
//! # Threads and stdin
//!
//! Keys are read here, under the arbiter's watcher token, exactly as the
//! turn-time keyboard watcher used to. That is what lets a mid-turn
//! permission prompt keep working unchanged: `PromptWindow` takes stdin, the
//! token is refused, this loop backs off; when the window closes the modal's
//! raw-mode guard has restored cooked mode, so raw mode is re-asserted on the
//! `suspended` false edge.
//!
//! # Who owns Esc
//!
//! Ctrl-C during a turn interrupts, and so does **Esc** — every press is
//! counted and acknowledged on screen (#2010), matching the watcher. This
//! file used to say *"(Esc belongs to
//! vi)"*, and that sentence was the whole of #2005: the classic surface had
//! shipped Esc-interrupt with the same tiers since `lib.rs`'s watcher, and the
//! cockpit deliberately declined to port it, leaving vi the one newt surface
//! where the conventional interrupt key did nothing. vim's own definition of
//! NORMAL-mode Esc, once nothing is pending, is a harmless no-op, so rung 7
//! costs the vi operator nothing they had.
//!
//! The order is a TABLE, not a call chain: `assets/esc_ladder.toml`, resolved
//! by [`Presenter::escapes`]. vi INSERT still owns Esc (it is an editing
//! transition), and so does a half-typed operator or count — that rung is
//! where newt beats codex, which kills the turn on a mid-turn `d` then Esc.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::style::{
    Attribute, Color as CColor, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{Clear, ClearType, EnableLineWrap};
use crossterm::{execute, queue};
use ratatui::backend::{Backend, ClearType as BackendClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::Rect;
use ratatui::layout::{Position, Size};
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::{Terminal, TerminalOptions, Viewport};

use super::ansi::{clip_to_width, visible_width, wrap_row, Row, TranscriptStream};
use super::pty::PtyCapture;
use crate::rich_input::{Chrome, EditorOutcome, MountedEditor, RichSurface, ScrollbackSink};
use crate::session_worker::{PanelMode, SurfaceRequest};
use crate::{InputSurface, ReadOutcome};

/// How long the loop sleeps in `poll` when nothing is happening. Bounds the
/// latency of a transcript byte, a keystroke, and a surface request alike.
const IDLE_POLL: Duration = Duration::from_millis(20);
/// The live clock in the header ticks at this cadence when idle (as before).
const CLOCK_TICK: Duration = Duration::from_millis(250);
/// After the session drops its end, keep draining the pty until it has been
/// quiet this long — the last lines it printed may still be in flight.
const DRAIN_QUIET: Duration = Duration::from_millis(120);

/// The real terminal, with autowrap switched off only while the cockpit writes.
///
/// A terminal decides whether to reflow its screen from the autowrap mode in
/// force *at the moment it resizes*. Left off at rest, every shrink truncates
/// the transcript for good (alacritty_terminal-based hosts such as herdr drop
/// the clipped cells), so the view crops smaller with each resize. Off during a
/// write keeps a miscounted glyph from wrapping the bottom-row footer and
/// scrolling the screen. Every write batch here ends in a flush, so fencing on
/// write/flush covers all of them without a per-site toggle.
struct WrapFence<W = File> {
    file: W,
    /// Shared by every clone: the mode belongs to the terminal, not a handle.
    state: Arc<FenceState>,
}

#[derive(Default)]
struct FenceState {
    wrap_off: AtomicBool,
    /// A lent-out modal or panel is writing; keep autowrap off until released.
    held: AtomicBool,
    /// An open frame (#3011): every clone's bytes collect here and reach the
    /// terminal as ONE synchronized write, so no compositor can paint the
    /// half-drawn state between `Hide` and ratatui's `Show`.
    frame: std::sync::Mutex<Option<Vec<u8>>>,
}

const WRAP_OFF: &[u8] = b"\x1b[?7l";
const WRAP_ON: &[u8] = b"\x1b[?7h";
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";

impl<W: Write> WrapFence<W> {
    fn new(file: W) -> Self {
        Self {
            file,
            state: Arc::default(),
        }
    }

    fn turn_off(&mut self) -> io::Result<()> {
        if !self.state.wrap_off.swap(true, Ordering::Relaxed) {
            self.emit(WRAP_OFF)?;
        }
        Ok(())
    }

    fn frame(&self) -> std::sync::MutexGuard<'_, Option<Vec<u8>>> {
        self.state.frame.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn emit(&mut self, bytes: &[u8]) -> io::Result<()> {
        // Disjoint fields: the lock borrows `state`, the write borrows `file`.
        let mut frame = self.state.frame.lock().unwrap_or_else(|p| p.into_inner());
        match frame.as_mut() {
            Some(frame) => {
                frame.extend_from_slice(bytes);
                Ok(())
            }
            None => self.file.write_all(bytes),
        }
    }

    fn begin_frame(&self) {
        *self.frame() = Some(SYNC_BEGIN.to_vec());
    }

    /// Send the open frame in one write, then flush as any batch would.
    fn end_frame(&mut self) -> io::Result<()> {
        let Some(mut frame) = self.frame().take() else {
            return Ok(());
        };
        frame.extend_from_slice(SYNC_END);
        self.file.write_all(&frame)?;
        self.flush()
    }

    fn hold(&mut self) -> io::Result<()> {
        self.state.held.store(true, Ordering::Relaxed);
        self.turn_off()?;
        self.file.flush()
    }

    fn release(&mut self) -> io::Result<()> {
        self.state.held.store(false, Ordering::Relaxed);
        self.flush()
    }
}

impl WrapFence {
    /// A plain handle for another surface. It writes outside this fence.
    fn try_clone(&self) -> io::Result<File> {
        self.file.try_clone()
    }

    fn fenced_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            state: Arc::clone(&self.state),
        })
    }
}

impl<W: Write> Write for WrapFence<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.turn_off()?;
        if self.frame().is_some() {
            self.emit(buf)?;
            return Ok(buf.len());
        }
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.frame().is_some() {
            // Deferred to `end_frame`, which flushes once for the whole frame.
            return Ok(());
        }
        if !self.state.held.load(Ordering::Relaxed)
            && self.state.wrap_off.swap(false, Ordering::Relaxed)
        {
            self.file.write_all(WRAP_ON)?;
        }
        self.file.flush()
    }
}

impl std::os::fd::AsRawFd for WrapFence {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.file.as_raw_fd()
    }
}

/// The caret ratatui last placed, as `y << 16 | x`, or [`NO_CARET`].
type CaretCell = Arc<std::sync::atomic::AtomicU32>;
const NO_CARET: u32 = u32::MAX;

/// A backend that remembers where ratatui last put the caret (#3014). A resize needs the
/// caret's row to turn the terminal's cursor reply back into the block's top, and ratatui
/// keeps that position private; this is the one place every draw reports it.
struct CaretBackend<W: Write = File> {
    inner: CrosstermBackend<WrapFence<W>>,
    caret: CaretCell,
}

impl<W: Write> Backend for CaretBackend<W> {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.caret.store(NO_CARET, Ordering::Relaxed);
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        self.caret.store(
            (u32::from(position.y) << 16) | u32::from(position.x),
            Ordering::Relaxed,
        );
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: BackendClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }
    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }
    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

/// Where the caret was the last time the block was fully drawn, and the geometry that draw
/// had. It answers a resize only while that geometry still holds (#3014).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CaretFrame {
    top: u16,
    block_h: u16,
    cols: u16,
    rows: u16,
    /// The caret's absolute row at that draw.
    y: u16,
    /// The caret's column: a narrower terminal may wrap the caret's own row before it.
    x: u16,
}

/// The screen: the real terminal, and where the block is on it.
///
/// Split from [`Presenter`] so the editor can be handed `&mut Screen` as its
/// [`ScrollbackSink`] while the presenter still borrows the editor.
struct Screen<W: Write = File> {
    tty: WrapFence<W>,
    term: Terminal<CaretBackend<W>>,
    caret: CaretCell,
    /// The caret at the last complete draw; see [`CaretFrame`].
    caret_frame: Option<CaretFrame>,
    /// Cleared the first time the terminal fails to answer a cursor query, so a terminal
    /// without DSR support pays for one wait, not one per resize.
    can_query_cursor: bool,
    cols: u16,
    rows: u16,
    /// First row of the block (0-based).
    top: u16,
    /// Rows the block occupies: `status_rows + viewport rows`.
    block_h: u16,
    /// 0 or 1: the row above the viewport carrying the session's in-progress
    /// line and the queued chip.
    status_rows: u16,
    /// The in-progress line as last fed by the stream.
    status: Row,
    /// Submits made while a turn ran, not yet consumed by a `ReadLine`.
    queued: usize,
    /// **The rows this block owns (#1980).** Replaces nothing — `top` and
    /// `block_h` stay, because the presenter needs them for its own arithmetic
    /// — but they are no longer PRIVATE bookkeeping: every move is reported to
    /// the arbiter, so another surface can no longer be handed these rows.
    region: newt_core::tty::RegionLease,
    /// Painted width of each block row in the last draw, top to bottom. A
    /// resize that narrows the terminal reflows any row wider than the new
    /// width onto extra rows, lifting the old block's top above `top`.
    painted_widths: Vec<usize>,
}

/// The block's rows, as the arbiter names them.
fn block_region(top: u16, block_h: u16) -> newt_core::tty::Region {
    newt_core::tty::Region::Rows {
        top,
        height: block_h,
    }
}

/// The rows a blocking window holds, bottom-anchored. It is a value the screen hands back
/// only through [`Screen::release_modal`], which clears the panel's rows and reclaims the
/// ones the panel scrolled the transcript out of in one step (#3012).
#[derive(Debug, PartialEq, Eq)]
#[must_use = "release a reservation with Screen::release_modal"]
struct ModalReservation {
    start: u16,
    rows: u16,
    chat_visible: bool,
    /// The first row below the transcript while the panel is up: the block's top when the
    /// panel opened, or `start` if the panel scrolled the transcript up to make room.
    /// Releasing moves the block back here, so the next transcript insert fills the rows
    /// the panel covered instead of leaving them blank in scrollback.
    transcript_end: u16,
}

/// The rows a blocking window holds until [`Presenter::finish_modal`] gives them back.
struct ModalRows {
    /// The height asked for; `u16::MAX` for a caller's own alternate screen, which takes it all.
    requested: u16,
    /// Rows reserved under the transcript; `None` on the alternate screen.
    reservation: Option<ModalReservation>,
    /// The alternate screen entered for the window. Dropping it leaves, on every exit path.
    alternate: Option<crate::transcript_pager::AltScreenGuard>,
}

/// Plan a bottom-anchored reservation of `requested` rows. `transcript_end` is the first row
/// below the transcript before the reservation (the block's top when a panel opens).
fn plan_modal_reservation(
    transcript_end: u16,
    screen_rows: u16,
    requested: u16,
) -> ModalReservation {
    let rows = requested.min(screen_rows);
    let start = screen_rows.saturating_sub(rows);
    ModalReservation {
        start,
        rows,
        chat_visible: false,
        transcript_end: transcript_end.min(start),
    }
}

/// Where the block's top returns to when a reservation is released: the end of the
/// transcript, clamped so the block still fits.
fn reclaimed_top(reservation: &ModalReservation, block_h: u16, screen_rows: u16) -> u16 {
    reservation
        .transcript_end
        .min(screen_rows.saturating_sub(block_h))
}

/// The bytes that reserve `plan` for a modal over a block at `top`: erase from the block
/// down, scroll the transcript rows the plan covers (`top - plan.start`) into scrollback,
/// and clear the modal's own rows.
fn modal_reserve_bytes(top: u16, screen_rows: u16, plan: &ModalReservation) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    queue!(
        buf,
        crossterm::cursor::Hide,
        MoveTo(0, top),
        Clear(ClearType::FromCursorDown)
    )?;
    let covered_transcript = top.saturating_sub(plan.start);
    if covered_transcript > 0 {
        queue!(buf, MoveTo(0, screen_rows.saturating_sub(1)))?;
        buf.extend(std::iter::repeat_n(b'\n', covered_transcript as usize));
    }
    queue!(buf, MoveTo(0, plan.start), Clear(ClearType::FromCursorDown))?;
    Ok(buf)
}

/// Clear the window's whole reservation, including rows above the editor's
/// fixed viewport, while retaining the transcript above it.
fn modal_cleanup_bytes(start: u16) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    queue!(buf, MoveTo(0, start), Clear(ClearType::FromCursorDown))?;
    Ok(buf)
}

impl Screen {
    fn terminal_size(&self) -> io::Result<(u16, u16)> {
        use std::os::fd::AsRawFd;
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // fd 1 is the capture PTY; query the saved real terminal instead.
        if unsafe { libc::ioctl(self.tty.as_raw_fd(), libc::TIOCGWINSZ, &mut size) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((size.ws_col.max(1), size.ws_row.max(1)))
    }

    fn viewport_rect(&self) -> Rect {
        Rect::new(
            0,
            self.top + self.status_rows,
            self.cols,
            self.block_h - self.status_rows,
        )
    }

    /// Output other than a draw has moved, or is about to move, the cursor off the caret: the
    /// last draw's [`CaretFrame`] no longer says where it is, and unchanged geometry does not
    /// either (a full screen scrolls under `insert_rows` with `top` and `block_h` intact).
    /// Only a fresh draw makes the frame trustworthy again (#3014).
    fn moved_cursor(&mut self) {
        self.caret_frame = None;
    }

    fn rebuild_term(&mut self) -> io::Result<()> {
        self.moved_cursor();
        let backend = CaretBackend {
            inner: CrosstermBackend::new(self.tty.fenced_clone()?),
            caret: Arc::clone(&self.caret),
        };
        self.term = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(self.viewport_rect()),
            },
        )?;
        self.term.clear()
    }

    /// Give the blocking window the composer's rows. Erase the editor first,
    /// then preserve any covered transcript in scrollback. The draft stays in
    /// memory and is painted again only after this window releases input.
    fn reserve_modal_rows(&mut self, requested: u16) -> io::Result<ModalReservation> {
        self.moved_cursor();
        let plan = plan_modal_reservation(self.top, self.rows, requested);
        self.tty.hold()?;
        let buf = modal_reserve_bytes(self.top, self.rows, &plan)?;
        self.tty.write_all(&buf)?;
        self.tty.flush()?;
        self.term.clear()?;
        Ok(plan)
    }

    /// Park the cursor on a blocking window's top row. Not [`Self::moved_cursor`]: an inline
    /// reservation has already said so, and the alternate screen keeps the primary cursor
    /// where the last draw left it, which is what a resize measures after (#3056).
    fn place_cursor(&mut self, row: u16) -> io::Result<()> {
        execute!(self.tty, MoveTo(0, row))?;
        self.tty.flush()
    }

    /// Release a panel's rows: clear them, and move the block back to the end of the
    /// transcript so the rows the panel scrolled the transcript out of are reclaimed rather
    /// than left blank in scrollback (#3012). The only way to release a reservation.
    fn release_modal(&mut self, reservation: &ModalReservation) -> io::Result<()> {
        self.moved_cursor();
        if reservation.chat_visible {
            return Ok(());
        }
        self.tty
            .write_all(&modal_cleanup_bytes(reservation.start)?)?;
        self.tty.flush()?;
        if self.reclaim_modal_rows(reservation) {
            self.rebuild_term()?;
        }
        Ok(())
    }

    /// Move the block's top back to where the transcript ended. `true` when it moved.
    fn reclaim_modal_rows(&mut self, reservation: &ModalReservation) -> bool {
        let top = reclaimed_top(reservation, self.block_h, self.rows);
        if top == self.top {
            return false;
        }
        self.top = top;
        // FORCED: the rows were already given back on the terminal.
        self.region.relocate(
            block_region(self.top, self.block_h),
            newt_core::tty::OnCollision::SuspendHolder,
        );
        true
    }

    /// Insert finished rows into the transcript above the block.
    ///
    /// The byte plan is [`render_insert`] (pure, testable); this writes it and
    /// re-seats the block viewport at its new top. All arithmetic, no queries —
    /// see the module docs.
    fn insert_rows(&mut self, rows: Vec<Row>) -> io::Result<()> {
        self.moved_cursor();
        let cols = self.cols as usize;
        let phys: Vec<Row> = rows.iter().flat_map(|r| wrap_row(r, cols)).collect();
        if phys.is_empty() || phys.len() > u16::MAX as usize {
            return Ok(());
        }
        let (buf, plan) = render_insert(self.top, self.block_h, self.rows, &phys)?;
        self.tty.write_all(&buf)?;
        self.tty.flush()?;
        let moved = plan.new_top != self.top;
        self.top = plan.new_top;
        // FORCED: the scroll has already happened on the terminal. See
        // `RegionLease::relocate` — refusing here would not un-scroll it.
        self.region.relocate(
            block_region(self.top, self.block_h),
            newt_core::tty::OnCollision::SuspendHolder,
        );
        if moved {
            self.rebuild_term()?;
        } else {
            self.term.clear()?;
        }
        Ok(())
    }

    /// Change the block's height (the editor grew, the status row appeared…).
    /// Scrolls the transcript up if the taller block would not fit below it.
    fn relayout(&mut self, editor_rows: u16, status_rows: u16) -> io::Result<()> {
        self.moved_cursor();
        let new_h = (editor_rows + status_rows).clamp(1, self.rows.max(1));
        if new_h == self.block_h && status_rows == self.status_rows {
            return Ok(());
        }
        let mut buf = Vec::new();
        queue!(buf, MoveTo(0, self.top), Clear(ClearType::FromCursorDown))?;
        if self.top + new_h > self.rows {
            let d = self.top + new_h - self.rows;
            queue!(buf, MoveTo(0, self.rows.saturating_sub(1)))?;
            buf.extend(std::iter::repeat_n(b'\n', d as usize));
            self.top = self.rows - new_h;
        }
        self.tty.write_all(&buf)?;
        self.tty.flush()?;
        self.block_h = new_h;
        // FORCED: a relayout is a redraw of a block that has already changed
        // height.
        self.region.relocate(
            block_region(self.top, self.block_h),
            newt_core::tty::OnCollision::SuspendHolder,
        );
        self.status_rows = status_rows;
        self.rebuild_term()
    }

    /// The terminal changed size: a grow pins the block to the new bottom
    /// ([`resize_new_top`]); otherwise it stays where the content ends while it
    /// still fits. The transcript above is the terminal's to reflow.
    fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        editor_rows: u16,
        status_rows: u16,
        measured_top: Option<u16>,
    ) -> io::Result<()> {
        // Narrowing reflows any old block row wider than the new terminal onto extra rows.
        let growth = |widths: &[usize]| {
            if cols.max(1) < self.cols {
                reflow_growth(widths, cols.max(1))
            } else {
                0
            }
        };
        // Where the block WAS, once the terminal has had its say. Measured (#3014): the
        // reply already includes where the reflow put the block, so only rows reflowed
        // BEFORE the caret (the rows above it, and its own row's text left of it) are still
        // unaccounted for; erasing a further `growth` rows up would eat transcript.
        // Unmeasured: the old top is right only when nothing moved, and the whole reflow
        // lifted the stale block above it.
        let measured_top = measured_top.zip(self.caret_frame).and_then(|(top, frame)| {
            let row = usize::from(frame.y.saturating_sub(frame.top)).min(self.painted_widths.len());
            let own = caret_row_wrap(
                frame.x,
                self.painted_widths.get(row).copied().unwrap_or(0),
                cols,
            )?;
            Some(top.saturating_sub(growth(&self.painted_widths[..row]).saturating_add(own)))
        });
        let (old_top, lifted) = match measured_top {
            Some(top) => (top, 0),
            None => (self.top, growth(&self.painted_widths)),
        };
        let old_rows = self.rows;
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        let new_h = (editor_rows + status_rows).clamp(1, self.rows);
        self.block_h = new_h;
        self.status_rows = status_rows;
        self.top = resized_top(measured_top, self.top, old_rows, self.rows, new_h);
        self.caret_frame = None;
        // FORCED: the TERMINAL resized; the block's new position is a
        // consequence, not a request.
        self.region.relocate(
            block_region(self.top, self.block_h),
            newt_core::tty::OnCollision::SuspendHolder,
        );
        // Erase from the topmost of the OLD and new block tops, not just the new
        // one (#4), raised by the rows the old block reflowed onto. Whichever
        // top is higher, nothing but block chrome sits below it (the block
        // never moves past its own old or new position), so this wipes every
        // stale status/editor row without losing transcript.
        let erase_from = resize_erase_from(old_top, self.top, self.rows).saturating_sub(lifted);
        let mut buf = Vec::new();
        queue!(buf, MoveTo(0, erase_from), Clear(ClearType::FromCursorDown))?;
        self.tty.write_all(&buf)?;
        self.tty.flush()?;
        self.rebuild_term()
    }

    /// Leave the terminal as a plain shell expects it: block erased, cursor
    /// at the block's top-left, wrap and cooked mode restored.
    fn shutdown(&mut self, trailing: &[u8]) -> io::Result<()> {
        let mut buf = Vec::new();
        queue!(buf, MoveTo(0, self.top), Clear(ClearType::FromCursorDown))?;
        if !trailing.is_empty() {
            buf.extend_from_slice(trailing);
            buf.extend_from_slice(b"\x1b[0m\r\n");
        }
        queue!(
            buf,
            EnableLineWrap,
            crossterm::event::DisableBracketedPaste,
            crossterm::cursor::Show
        )?;
        self.tty.write_all(&buf)?;
        self.tty.flush()?;
        // Raw mode is NOT released here. The session guard's doc already says
        // "the MODES are this guard's job"; disabling here as well restored
        // crossterm's global early, while the capture was still installed, and
        // then the guard restored again. One owner, one restore (#1925).
        Ok(())
    }
}

impl<W: Write> Screen<W> {
    /// Paint the block as one frame (#3011): status row and viewport reach the
    /// terminal in a single synchronized write, sent even if drawing failed.
    fn draw(
        &mut self,
        editor: &MountedEditor,
        chrome: Chrome<'_>,
        chat_inactive: bool,
    ) -> io::Result<()> {
        self.tty.begin_frame();
        let drawn = self.draw_frame(editor, chrome, chat_inactive);
        let sent = self.tty.end_frame();
        drawn.and(sent)
    }

    /// The status row (raw, clipped, with the queued chip at the right edge),
    /// then the ratatui viewport.
    fn draw_frame(
        &mut self,
        editor: &MountedEditor,
        chrome: Chrome<'_>,
        chat_inactive: bool,
    ) -> io::Result<()> {
        let mut widths = Vec::with_capacity(self.block_h as usize);
        if self.status_rows == 1 {
            let cols = self.cols as usize;
            let chip = if self.queued > 0 {
                format!(" ⏎ queued: {}", self.queued)
            } else {
                String::new()
            };
            let chip_w = newt_core::tty::str_width(&chip);
            let avail = cols.saturating_sub(chip_w + 1);
            let status = clip_to_width(&self.status, avail);
            widths.push(if chip.is_empty() {
                visible_width(&status)
            } else {
                cols
            });
            let mut buf = Vec::new();
            // Hidden across the two writes; ratatui's own draw shows it again
            // at the editor's caret, so it never flickers to the status row.
            queue!(buf, crossterm::cursor::Hide, MoveTo(0, self.top))?;
            buf.extend_from_slice(&status);
            queue!(
                buf,
                SetAttribute(Attribute::Reset),
                ResetColor,
                Clear(ClearType::UntilNewLine)
            )?;
            if !chip.is_empty() {
                let x = (cols - chip_w) as u16;
                queue!(
                    buf,
                    MoveTo(x, self.top),
                    SetForegroundColor(CColor::DarkGrey),
                    crossterm::style::Print(chip),
                    ResetColor
                )?;
            }
            self.tty.write_all(&buf)?;
            self.tty.flush()?;
        }
        self.caret.store(NO_CARET, Ordering::Relaxed);
        let frame = self.term.draw(|f| editor.draw(f, chrome, chat_inactive))?;
        let area = frame.area;
        let placed = self.caret.load(Ordering::Relaxed);
        self.caret_frame = (placed != NO_CARET).then_some(CaretFrame {
            top: self.top,
            block_h: self.block_h,
            cols: self.cols,
            rows: self.rows,
            y: (placed >> 16) as u16,
            x: (placed & 0xFFFF) as u16,
        });
        widths.extend((area.top()..area.bottom()).map(|y| {
            (area.left()..area.right())
                .rev()
                .find(|&x| frame.buffer[(x, y)].symbol() != " ")
                .map_or(0, |x| usize::from(x - area.left()) + 1)
        }));
        self.painted_widths = widths;
        Ok(())
    }
}

impl ScrollbackSink for Screen {
    fn insert(&mut self, lines: Vec<Line<'static>>) -> io::Result<()> {
        let rows = lines
            .iter()
            .map(line_to_ansi)
            .collect::<io::Result<Vec<_>>>()?;
        self.insert_rows(rows)
    }
}

/// Where the block goes after `k` physical rows are inserted at `top`.
#[derive(Debug, PartialEq, Eq)]
struct InsertPlan {
    /// Newlines to emit at the bottom row after the rows, so the last row
    /// ends up just above the block.
    extra_scroll: u16,
    new_top: u16,
}

/// Build the exact byte sequence that lays `phys` finished rows into the
/// transcript above a block at `top`, plus where the block lands. Pure so the
/// scroll bytes can be pinned without a terminal.
///
/// **Rows are separated by `\r\n`, never terminated by one (#2).** A trailing
/// `\r\n` after the last row, once that row already sits on the bottom line,
/// costs one extra bottom-row scroll — which pushes the just-written rows up
/// and opens a blank gap between the transcript and the block. The block is
/// repositioned solely by `plan.extra_scroll` line feeds at the bottom row, and
/// [`plan_insert`] already accounts for the writing that happens without that
/// stray terminator.
fn render_insert(
    top: u16,
    block_h: u16,
    rows: u16,
    phys: &[Row],
) -> io::Result<(Vec<u8>, InsertPlan)> {
    let k = phys.len() as u16;
    let plan = plan_insert(top, block_h, rows, k);
    let mut buf = Vec::with_capacity(phys.iter().map(Vec::len).sum::<usize>() + 64);
    queue!(buf, MoveTo(0, top), Clear(ClearType::FromCursorDown))?;
    for (i, row) in phys.iter().enumerate() {
        if i > 0 {
            // Between rows, not after the last one.
            buf.extend_from_slice(b"\r\n");
        }
        buf.extend_from_slice(row);
        // A reset per row: styling from the transcript must never leak into the
        // next row, into a scrolled-in blank line, or into the block.
        buf.extend_from_slice(b"\x1b[0m");
    }
    if plan.extra_scroll > 0 {
        queue!(buf, MoveTo(0, rows.saturating_sub(1)))?;
        buf.extend(std::iter::repeat_n(b'\n', plan.extra_scroll as usize));
    }
    Ok((buf, plan))
}

/// Emit the mode restores `open` must undo — line wrap back on, bracketed
/// paste off, cursor shown — to `w`. Split from [`restore_terminal_modes`] so
/// the exact sequence is unit-testable against a buffer (`io::stdout` is
/// captured by the test harness and cannot be read back).
fn write_mode_restores(w: &mut impl io::Write) -> io::Result<()> {
    // Queue into an owned buffer (the exact idiom `Screen::shutdown` uses),
    // then write it — so this composes over any `Write`, `io::stdout()` or a
    // test's `Vec`, without depending on the macro's reborrow of a `&mut`.
    let mut buf = Vec::new();
    queue!(
        buf,
        EnableLineWrap,
        crossterm::event::DisableBracketedPaste,
        crossterm::cursor::Show
    )?;
    w.write_all(&buf)
}

/// Put the terminal modes `open` took back: raw mode off, then the sequence
/// above written to `io::stdout()` — the real terminal once the capture has
/// dropped. This is the body of the session's [`RestoreOnDrop`] guard, named so
/// it has one definition the guard and the test share. Errors are swallowed:
/// a Drop path cannot propagate, and a best-effort restore beats none.
fn restore_terminal_modes() {
    // Escape sequences ONLY. Raw mode is the `_raw: RawModeGuard` field's, and
    // it is declared after this guard so it restores AFTER these — see that
    // field's doc for why the order matters.
    let _ = write_mode_restores(&mut io::stdout());
}

/// Where the block moves to on a resize event (#2441, #2632, #3014).
///
/// **A terminal that GROWS pins the block to the new bottom** (maintainer decision on
/// #3014 item 2). Terminals disagree on what a grow does to the transcript (some add rows
/// below it, some pull it down), but the prompt stays where the operator's eyes and the
/// keyboard are, the bottom row; the gap a bottom-adding terminal leaves above it is the
/// accepted cost. This reverses the #2632 "stay under the content" rule for grows only.
///
/// Every other resize (same size, or a shrink) leaves the transcript where it was, so
/// `old_top` is still ground truth there. Move the block only when it no longer fits at
/// `old_top`, and bottom-anchor it then.
fn resize_new_top(old_top: u16, old_rows: u16, new_rows: u16, new_h: u16) -> u16 {
    if new_rows <= old_rows && old_top.saturating_add(new_h) <= new_rows {
        old_top
    } else {
        new_rows.saturating_sub(new_h)
    }
}

/// The block's top after a resize: a grow pins it to the bottom; otherwise the position the
/// terminal reported when it was measured (#3014), else [`resize_new_top`]'s guess from the
/// old top. Either way the block must fit the new screen.
fn resized_top(
    measured: Option<u16>,
    current_top: u16,
    old_rows: u16,
    new_rows: u16,
    new_h: u16,
) -> u16 {
    resize_new_top(measured.unwrap_or(current_top), old_rows, new_rows, new_h)
}

/// The block's top, from the row the terminal says its cursor is on and where the caret sits
/// inside the block (`caret_offset` rows below the top). `None` when the reply is above the
/// caret's own offset (a reply that cannot be this block's).
fn block_top_from_reply(cursor_row: u16, caret_offset: u16) -> Option<u16> {
    cursor_row.checked_sub(caret_offset)
}

/// How many rows further down a terminal `cols` wide reflows a caret at column `x` of a row
/// whose text is `width` wide: the caret travels with the text left of it. `None` when the
/// caret sits past the text and past the new width, where terminals disagree.
fn caret_row_wrap(x: u16, width: usize, cols: u16) -> Option<u16> {
    let cols = cols.max(1);
    if x < cols {
        Some(0)
    } else if usize::from(x) < width {
        Some(x / cols)
    } else {
        None
    }
}

/// The row `resize` clears downward from so the OLD cockpit region cannot
/// survive above the new block (#4). The block may now stay put instead of
/// re-anchoring to the bottom (#2632), so the topmost of the two tops is what
/// covers both regions, not a shared bottom anchor; clamped into the
/// (possibly smaller) new screen.
fn resize_erase_from(old_top: u16, new_top: u16, rows: u16) -> u16 {
    old_top.min(new_top).min(rows.saturating_sub(1))
}

/// How many rows the old block grows by when a terminal reflows it at `cols`:
/// each row `w` wide now takes `ceil(w / cols)` rows instead of one.
fn reflow_growth(widths: &[usize], cols: u16) -> u16 {
    let cols = usize::from(cols.max(1));
    let extra: usize = widths
        .iter()
        .map(|w| w.div_ceil(cols).saturating_sub(1))
        .sum();
    u16::try_from(extra).unwrap_or(u16::MAX)
}

/// Pure geometry — the part of `insert_rows` that must be exactly right and
/// can be pinned without a terminal.
///
/// Rows are written from `top` as `\r\n`-separated lines; a `\r\n` issued while
/// on the last screen row scrolls by one. Afterwards the last written row sits
/// at `min(top+k-1, rows-1)`; the block wants to start right after it, but no
/// lower than `rows - block_h`, so whatever overshoot there is becomes extra
/// scroll.
fn plan_insert(top: u16, block_h: u16, rows: u16, k: u16) -> InsertPlan {
    let last_row = (top as u32 + k as u32 - 1).min(rows.saturating_sub(1) as u32) as u16;
    let floor = rows.saturating_sub(block_h);
    let after = last_row + 1;
    if after > floor {
        InsertPlan {
            extra_scroll: after - floor,
            new_top: floor,
        }
    } else {
        InsertPlan {
            extra_scroll: 0,
            new_top: after,
        }
    }
}

/// A ratatui line as raw ANSI: the echoed `[stamp]` / `› body` / note rows
/// the editor commits. Foreground colour, bold and dim are all these use.
fn line_to_ansi(line: &Line<'_>) -> io::Result<Row> {
    let mut out = Vec::new();
    for span in &line.spans {
        if let Some(fg) = span.style.fg {
            queue!(out, SetForegroundColor(CColor::from(fg)))?;
        }
        if let Some(bg) = span.style.bg {
            queue!(out, SetBackgroundColor(CColor::from(bg)))?;
        }
        if span.style.add_modifier.contains(Modifier::BOLD) {
            queue!(out, SetAttribute(Attribute::Bold))?;
        }
        if span.style.add_modifier.contains(Modifier::DIM) {
            queue!(out, SetAttribute(Attribute::Dim))?;
        }
        out.extend_from_slice(span.content.as_bytes());
        queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
    }
    Ok(out)
}

/// A turn in flight: the flag the session races its work against.
struct Turn {
    cancel: Arc<AtomicBool>,
}

/// The cockpit's owner of the terminal, the keyboard and the editor.
pub(crate) struct Presenter {
    surface: RichSurface,
    editor: MountedEditor,
    screen: Screen,
    capture: PtyCapture,
    stream: TranscriptStream,
    pending_read: Option<SyncSender<anyhow::Result<ReadOutcome>>>,
    #[cfg(feature = "live-spill")]
    spills: Option<Arc<crate::completed_spill::CompletedSpillArchive>>,
    queued: VecDeque<String>,
    turn: Option<Turn>,
    /// The `suspended` edge detector for re-asserting raw mode after a modal.
    /// The registration is weak, so the target must be held alongside it.
    arbiter: newt_core::tty::EphemeralRegistration,
    _arbiter_target: Arc<dyn newt_core::tty::Ephemeral>,
    was_suspended: bool,
    /// The session is serving a command the operator typed (a line starting `/`): set when one
    /// is handed over, cleared when the session asks for input again. A turn or operation the
    /// command runs does not clear it, so a window the command reopens afterwards is still the
    /// operator's (#3056); see [`Self::take_modal_rows`].
    serving_command: bool,
    /// True only while a blocking window owns the keyboard. The editor draft
    /// stays mounted in memory and is hidden until the window closes.
    chat_inactive: bool,
    dirty: bool,
    last_draw: Instant,
    /// The meta prefix at the prompt (`ctrl+space` then a key), fed before the
    /// editor sees a key — the same sequencer and table panels use.
    meta: crate::prefix::Sequencer,
    /// Restores the terminal modes `open` took — raw mode, line wrap, bracketed
    /// paste, cursor visibility — on EVERY exit of the session: a clean return,
    /// an `io::Error` propagating out of `run`, or a panic (via Drop during
    /// unwind, the crate's `MouseCaptureGuard` precedent). `Screen::shutdown`
    /// still does the visible teardown on the clean path, but the MODES are this
    /// guard's job so a `?` or panic before `shutdown` cannot strand the terminal
    /// raw / no-wrap / paste-on. Declared LAST so it drops AFTER `capture`, i.e.
    /// once fd 1 is back on the real terminal, letting its `execute!` land there
    /// rather than in the pty. Reuses `RestoreOnDrop` (#1411 convention).
    _restore: crate::RestoreOnDrop<fn()>,
    /// Raw mode, restored to the termios this session FOUND (#1925).
    ///
    /// It used to be `disable_raw_mode()` inside `_restore`'s closure, and
    /// crossterm keeps ONE process-global "mode prior to raw" — so the cockpit
    /// restored to whatever the process last had rather than to what it took.
    /// C2b (#1920) hit that as a real failure: an inner frame handing the
    /// terminal back while an outer one was still up.
    ///
    /// DECLARED LAST, AFTER `_restore`, AND THAT IS THE SECOND FIX. Fields
    /// drop in declaration order, so the escape-sequence restores (line wrap,
    /// bracketed paste, cursor) now run BEFORE raw mode is given back. The old
    /// `restore_terminal_modes` did raw FIRST — the inverse of the order #1901
    /// argued for, where releasing line discipline while paste markers are
    /// still armed lets a paste in that window arrive as a literal `ESC[200~`.
    /// Composition fixes it without a line of ordering code.
    _raw: newt_core::tty::raw_mode::RawModeGuard,
}

/// The seam `esc_ladder_pty_test`'s child half drives the cockpit through.
///
/// It exists because that test cannot use [`Presenter::run`] — `run` needs a
/// live session channel — and must not open-code the loop body, which would be
/// a second implementation of the thing under test. Two methods, both thin:
/// one turn of the loop, and the exact predicate input the production arm
/// reads. Everything else the test needs is already `pub(crate)`.
#[cfg(test)]
impl Presenter {
    /// One turn of [`Presenter::run`]'s body, minus the request channel:
    /// relay whatever the session printed, take one bounded look at the
    /// keyboard, repaint.
    pub(crate) fn pump(&mut self) -> io::Result<()> {
        self.drain_pty()?;
        self.poll_keys()?;
        self.draw()
    }

    /// The live claim set — the same value [`Presenter::escapes`] resolves
    /// against, so the test observes the production input rather than a
    /// test-only twin of it.
    pub(crate) fn claims(&self) -> precedence_ladder::ClaimSet {
        self.editor.claim_set()
    }
}

/// The cockpit does not paint through the arbiter — its rows are on the real
/// terminal, outside the pty the arbiter's writers see — but registering
/// gives it the one thing it needs from the arbiter: the `suspended` edge.
struct NoOpEphemeral;
impl newt_core::tty::Ephemeral for NoOpEphemeral {
    fn erase(&self) {}
    fn restore(&self) {}
}

impl Presenter {
    /// Take the terminal. Everything that needs the REAL fd 1 happens here,
    /// before the capture: the size, the one cursor query, raw mode, wrap
    /// off. Fails closed — on any error nothing has been captured and the
    /// caller falls back to the classic surface.
    pub(crate) fn open(surface: RichSurface) -> io::Result<Self> {
        let (cols, rows) = crossterm::terminal::size()?;
        let (cols, rows) = (cols.max(1), rows.max(1));
        // #1950: the same one answer the inline surfaces use. This call
        // used to be the cockpit's own `?` — a quiet terminal meant the
        // cockpit never opened, which is the same defect as a panel that
        // never opens, and it must not be a second implementation of the
        // fallback.
        let cursor = crate::inline_viewport::cursor_position_or_anchor();
        let (x, y) = (cursor.x, cursor.y);
        let mut editor = MountedEditor::new(
            surface.edit(),
            surface.gutter(),
            surface.history(),
            crate::type_ahead::take().trim_end_matches('\n'),
        );
        let editor_rows = editor.wanted_rows(cols, rows, &surface.chrome());
        let block_h = editor_rows.clamp(1, rows);
        // Start on a fresh row, then make room for the block below it.
        let mut stdout = io::stdout();
        let mut y = y;
        if x > 0 {
            stdout.write_all(b"\r\n")?;
            y = (y + 1).min(rows - 1);
        }
        let top = if y + block_h > rows {
            let d = y + block_h - rows;
            execute!(stdout, MoveTo(0, rows - 1))?;
            for _ in 0..d {
                stdout.write_all(b"\n")?;
            }
            stdout.flush()?;
            rows - block_h
        } else {
            y
        };
        let raw = newt_core::tty::raw_mode::RawModeGuard::enter()?;
        execute!(stdout, crossterm::event::EnableBracketedPaste)?;
        // The terminal's modes are now taken. Bind their restore the instant
        // after — and crucially BEFORE the fallible capture install below — so
        // that no `?`, error, or panic between here and a clean `shutdown` can
        // leave the terminal raw, wrap off, bracketed paste on, cursor hidden.
        // The bug is made unrepresentable, not fixed per-path (#1411): the
        // terminal cannot be taken without binding something that gives it back.
        // Non-capturing closure → `fn()`. It writes to `io::stdout()`, which is
        // the pty while the capture is installed and the real terminal again
        // once `capture` has dropped — and this guard is the last-declared field,
        // so it always drops after `capture`.
        let restore: crate::RestoreOnDrop<fn()> = crate::RestoreOnDrop {
            restore: restore_terminal_modes,
        };
        let capture = PtyCapture::install(cols, rows)?;
        let tty = WrapFence::new(capture.tty().try_clone()?);
        let caret: CaretCell = Arc::new(std::sync::atomic::AtomicU32::new(NO_CARET));
        let backend = CaretBackend {
            inner: CrosstermBackend::new(tty.fenced_clone()?),
            caret: Arc::clone(&caret),
        };
        let term = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, top, cols, block_h)),
            },
        )?;
        // The INITIAL take is a request and may honestly fail: the cockpit is
        // the session's base surface, so anything already holding these rows
        // means something is wrong, and starting anyway would reproduce the
        // overpainting this sweep exists to end. Subsequent moves are reports,
        // not requests — see the `relocate` calls above.
        let region = newt_core::tty::Terminal::lease_region(
            block_region(top, block_h),
            newt_core::tty::OnCollision::Refuse,
        )
        .ok_or_else(|| io::Error::other("another surface already owns the cockpit's rows"))?;
        let mut screen = Screen {
            tty,
            term,
            caret,
            caret_frame: None,
            can_query_cursor: true,
            cols,
            rows,
            top,
            block_h,
            status_rows: 0,
            status: Vec::new(),
            queued: 0,
            region,
            painted_widths: Vec::new(),
        };
        screen.term.clear()?;
        let ephemeral: Arc<dyn newt_core::tty::Ephemeral> = Arc::new(NoOpEphemeral);
        let arbiter = newt_core::tty::Terminal::register_ephemeral(&ephemeral);
        Ok(Self {
            surface,
            editor,
            screen,
            capture,
            stream: TranscriptStream::new(),
            pending_read: None,
            #[cfg(feature = "live-spill")]
            spills: None,
            queued: VecDeque::new(),
            turn: None,
            arbiter,
            _arbiter_target: ephemeral,
            was_suspended: false,
            serving_command: false,
            chat_inactive: false,
            dirty: true,
            meta: crate::prefix::Sequencer::new(crate::prefix::current()),
            last_draw: Instant::now(),
            _restore: restore,
            _raw: raw,
        })
    }

    /// Serve the session until it drops its end of the channel, then leave
    /// the terminal clean. The pump ending IS the session ending.
    pub(crate) fn run(mut self, requests: &Receiver<SurfaceRequest>) -> io::Result<()> {
        loop {
            loop {
                match requests.try_recv() {
                    Ok(req) => self.handle_request(req)?,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return self.finish(),
                }
            }
            self.drain_pty()?;
            self.poll_keys()?;
            self.sync_modal_edge();
            if self.dirty || self.last_draw.elapsed() >= CLOCK_TICK {
                self.draw()?;
            }
        }
    }

    fn finish(mut self) -> io::Result<()> {
        // Let the session's last bytes land.
        let mut quiet_since = Instant::now();
        while quiet_since.elapsed() < DRAIN_QUIET {
            if self.drain_pty()? {
                quiet_since = Instant::now();
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let trailing = self.stream.partial().to_vec();
        self.screen.shutdown(&trailing)?;
        // `capture` drops after this returns: fd 1/2 restored.
        Ok(())
    }

    pub(crate) fn handle_request(&mut self, req: SurfaceRequest) -> io::Result<()> {
        match req {
            SurfaceRequest::ReadLine { prompt: _, reply } => {
                self.serving_command = false;
                // A confirmed `:wq` submitted its turn last time; now that the
                // turn has run, end the conversation and exit before reading
                // anything new — same order as the classic surface.
                if self.surface.take_end_quit() {
                    let _ = reply.send(Ok(ReadOutcome::EndAndQuit));
                } else if let Some(line) = self.queued.pop_front() {
                    self.screen.queued = self.queued.len();
                    self.hand_over(&line);
                    let _ = reply.send(Ok(ReadOutcome::Line(line)));
                } else {
                    self.pending_read = Some(reply);
                }
                self.dirty = true;
            }
            // #2524 item 7: the cockpit's own terminal handoff, mirroring
            // `Interact` below rather than `ReadLine` above — a pending
            // clarification takes the modal's reserved rows the same way a
            // permission prompt does, instead of the persistently-mounted
            // chat editor `ReadLine` queues into.
            SurfaceRequest::PresentClarification {
                batch,
                hint,
                prompt: _,
                color: _,
                verbose: _,
                reply,
            } => {
                // The session is asking, not serving a command: the question
                // stays inline, under the turn that asked it.
                self.serving_command = false;
                if self.surface.take_end_quit() {
                    let _ = reply.send(Ok(ReadOutcome::EndAndQuit));
                    self.dirty = true;
                    return Ok(());
                }
                let prompt_output = self.screen.tty.try_clone()?;
                if self.dirty {
                    self.draw()?;
                }
                let requested_rows =
                    crate::clarification_modal::requested_rows(&batch, self.screen.cols);
                let rows = self.take_modal_rows(requested_rows)?;
                let area = self.lent_area(&rows);
                self.chat_inactive = true;
                if rows.reservation.as_ref().is_some_and(|r| r.chat_visible) {
                    if let Err(error) = self.draw() {
                        self.chat_inactive = false;
                        return Err(error);
                    }
                }
                if let Err(error) = self.screen.place_cursor(area.y) {
                    let _ = self.finish_modal(Some(rows));
                    let _ = self.draw();
                    return Err(error);
                }
                let window = Self::suspend_terminal(prompt_output);
                let result: io::Result<ReadOutcome> = self
                    .screen
                    .tty
                    .try_clone()
                    .and_then(|out| crate::inline_viewport::cockpit_panel_terminal(out, area))
                    .and_then(|mut terminal| {
                        crate::clarification_modal::present_in(&mut terminal, &batch, &hint)
                    });
                drop(window);
                let modal_cleanup = self.finish_modal(Some(rows));
                let repaint = (|| -> io::Result<()> {
                    self.screen.term.clear()?;
                    self.draw()
                })();
                let _ = reply.send(result.map_err(anyhow::Error::from));
                modal_cleanup?;
                repaint?;
            }
            SurfaceRequest::Reload { reply } => {
                let result = self.surface.reload();
                let draft = self.editor.draft();
                // #2006: a `/vi`·`/emacs`·`/nano` reload rebuilds the mount,
                // and the vi mode/jumplist/`;`-target ride across it the same
                // way the draft above does.
                let vi = self.editor.take_vi();
                self.editor = MountedEditor::new(
                    self.surface.edit(),
                    self.surface.gutter(),
                    self.surface.history(),
                    &draft,
                );
                self.editor.adopt_vi(vi);
                self.editor.set_turn_running(self.turn.is_some());
                let _ = reply.send(result);
                self.dirty = true;
            }
            SurfaceRequest::AddHistory(entry) => {
                self.surface.add_history(&entry);
                self.editor.set_history(self.surface.history());
            }
            SurfaceRequest::SaveHistory => self.surface.save_history(),
            SurfaceRequest::SetRuntimeContext {
                model,
                endpoint,
                gauge,
                session,
            } => {
                self.surface
                    .set_runtime_context(&model, &endpoint, gauge, &session);
                self.dirty = true;
            }
            SurfaceRequest::SetBackgroundJobs(jobs) => {
                self.surface.set_background_jobs(jobs);
                self.dirty = true;
            }
            SurfaceRequest::SetToolPolicy(label) => {
                self.surface.set_tool_policy(label);
                self.dirty = true;
            }
            #[cfg(feature = "live-spill")]
            SurfaceRequest::SetSpillArchive(archive) => {
                archive.enable_inspection();
                self.spills = Some(archive);
                self.screen
                    .insert_rows(vec![b"F4 inspect retained output (or /spill open)".to_vec()])?;
            }
            SurfaceRequest::SetTabs(tabs) => {
                self.surface.set_tabs(tabs);
                self.dirty = true;
            }
            SurfaceRequest::TurnStarted { cancel } => {
                self.turn = Some(Turn { cancel });
                // #2006: the mode hint may advertise `^C interrupt` exactly
                // while that is true.
                self.editor.set_turn_running(true);
                self.dirty = true;
            }
            SurfaceRequest::RunBang {
                command,
                color,
                verbose,
                reply,
            } => {
                // Serve synchronously: this loop cannot consume a single key
                // while the foreground command owns the real terminal.
                let result = self.run_bang_command(&command, color, verbose);
                let _ = reply.send(result.map_err(Into::into));
            }
            // C1 (#1862): the cockpit owns the terminal, so it presents the
            // interaction itself. `suspend_for_prompt` takes the terminal from
            // under the cockpit and restores it on drop — the path #1770 fixed
            // and `presenter`'s own PTY test exercises.
            SurfaceRequest::Interact { interaction, reply } => {
                // Clone the real terminal before changing any focus state. If
                // this rare allocation fails, the request's reply sender drops
                // cleanly and chat never gets stranded in its inactive style.
                let prompt_output = self.screen.tty.try_clone()?;
                // Requests are drained in a batch before the loop's normal
                // draw. Apply any pending editor/status geometry first so the
                // modal reserves rows against the block that will actually be
                // painted, not the stale top from before (for example) a
                // background-job row appeared.
                if self.dirty {
                    self.draw()?;
                }
                let requested_rows =
                    crate::interaction_view::requested_rows(&interaction, self.screen.cols);
                let rows = self.take_modal_rows(requested_rows)?;
                let area = self.lent_area(&rows);
                // The modal is the only visible input surface until it exits.
                self.chat_inactive = true;
                if rows.reservation.as_ref().is_some_and(|r| r.chat_visible) {
                    if let Err(error) = self.draw() {
                        self.chat_inactive = false;
                        return Err(error);
                    }
                }
                if let Err(error) = self.screen.place_cursor(area.y) {
                    let _ = self.finish_modal(Some(rows));
                    let _ = self.draw();
                    return Err(error);
                }
                // fd 1 is the cockpit's captured PTY. Route this blocking
                // interaction to the saved real terminal instead, otherwise
                // its bytes would wait in the capture until this same loop
                // returned from the read — a prompt visible only after it was
                // answered.
                let window = Self::suspend_terminal(prompt_output);
                let rich = self.screen.tty.try_clone().and_then(|out| {
                    let terminal = crate::inline_viewport::cockpit_panel_terminal(out, area)?;
                    crate::interaction_view::present_in(terminal, &interaction)
                });
                let (outcome, prompt_notice) = match rich {
                    Ok((outcome, _)) => crate::permissions::apply_chat_prompt_policy(outcome),
                    Err(_) => crate::permissions::present_on_terminal_with_width(
                        &window,
                        &interaction,
                        usize::from(self.screen.cols),
                    ),
                };
                drop(window);
                let modal_cleanup = self.finish_modal(Some(rows));
                // The modal wrote outside ratatui's diff. Repaint the inline
                // region from a clean buffer so focus returns to chat without
                // leaving modal bytes or a dim chevron behind. Send the answer
                // even if that cosmetic repaint fails, so the session cannot
                // remain blocked waiting on a result it already supplied.
                let repaint = (|| {
                    self.screen.term.clear()?;
                    if let Some(notice) = prompt_notice {
                        // A slash typed at a modal belongs at the chat prompt.
                        // Commit that guidance above the fixed viewport after
                        // the modal closes so the repaint cannot swallow it.
                        self.screen.insert_rows(vec![notice.as_bytes().to_vec()])?;
                    }
                    self.draw()
                })();
                let _ = reply.send(outcome);
                modal_cleanup?;
                repaint?;
            }
            // The panel sibling of `Interact`: lend the real terminal and
            // park while the session draws. Inline panels take rows like any
            // blocking window; alternate-screen panels preserve the primary
            // buffer themselves. Both use the same focus transfer and release
            // channel.
            SurfaceRequest::Panel { mode, reply } => {
                let panel_output = match self.screen.tty.try_clone() {
                    Ok(tty) => tty,
                    // A failed clone is not fatal: tell the session there are
                    // no rows and let it keep its own path, exactly as a lean
                    // surface would answer.
                    Err(_) => {
                        let _ = reply.send(None);
                        return Ok(());
                    }
                };
                // Apply pending geometry first, so the panel reserves rows
                // against the block that will actually be painted.
                if self.dirty {
                    self.draw()?;
                }
                let mut rows = match mode {
                    PanelMode::Inline(requested) => self.take_modal_rows(requested)?,
                    // The caller's alternate buffer occludes the composer and
                    // saves the primary screen. Do not erase or scroll it first.
                    PanelMode::AlternateScreen => ModalRows {
                        requested: u16::MAX,
                        reservation: None,
                        alternate: None,
                    },
                };
                self.chat_inactive = true;
                if rows
                    .reservation
                    .as_ref()
                    .is_some_and(|window| window.chat_visible)
                {
                    if let Err(error) = self.draw() {
                        self.chat_inactive = false;
                        return Err(error);
                    }
                }
                // The panel owns the keyboard from here. `released` wakes this
                // thread when the window drops — normally, on `?`, or on an
                // unwind — so the rows cannot be stranded by a panel that
                // returns through a path nobody thought about.
                let (release, released) = std::sync::mpsc::sync_channel(1);
                let window = crate::session_worker::PanelWindow::new(
                    panel_output,
                    self.lent_area(&rows),
                    Some(release),
                );
                if reply.send(Some(window)).is_err() {
                    // The session vanished between asking and receiving. Undo
                    // the reservation rather than parking forever.
                    let cleanup = self.finish_modal(Some(rows));
                    let _ = self.screen.term.clear();
                    let _ = self.draw();
                    return cleanup;
                }
                // Park. A `RecvError` means the window was dropped without a
                // send — the same "the panel is done" signal, reached by a
                // path that could not send. Either way: clean up.
                // #2571: while parked, answer re-measure requests — the panel
                // hears the resize (it owns the keyboard), this thread owns
                // the layout. A `RecvError` is the drop: the panel is done.
                while let Ok(crate::session_worker::PanelSignal::Remeasure { rows: asked, reply }) =
                    released.recv()
                {
                    // The operator sized the panel (Shift-↑/↓, zoom): one on
                    // the alternate screen takes the new request, which
                    // `lent_area` clamps.
                    if let (Some(asked), Some(_)) = (asked, &rows.alternate) {
                        rows.requested = asked;
                    }
                    // A failed re-measure keeps the old region rather than
                    // stranding the panel: the reply still goes out.
                    let _ = self.remeasure_panel(&rows);
                    let _ = reply.send(self.lent_area(&rows));
                }
                let modal_cleanup = self.finish_modal(Some(rows));
                // The panel wrote outside ratatui's diff, so the mounted block
                // is repainted from a clean buffer — the same restore the
                // modal path performs, and the reason the header comes back.
                let repaint = (|| {
                    self.screen.term.clear()?;
                    self.draw()
                })();
                modal_cleanup?;
                repaint?;
            }
            SurfaceRequest::TurnEnded => {
                self.turn = None;
                self.editor.set_turn_running(false);
                newt_core::tty::set_interrupt_pending(false);
                self.dirty = true;
            }
        }
        Ok(())
    }

    /// Read what the session has printed. `Ok(true)` when anything arrived.
    fn drain_pty(&mut self) -> io::Result<bool> {
        let mut any = false;
        let mut buf = [0u8; 8192];
        loop {
            let mut pfd = libc::pollfd {
                fd: self.capture.master_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll on one descriptor we own, zero timeout.
            let ready = unsafe { libc::poll(&mut pfd, 1, 0) };
            if ready <= 0 || pfd.revents & libc::POLLIN == 0 {
                break;
            }
            let n = self.capture.read_available(&mut buf)?;
            if n == 0 {
                break;
            }
            any = true;
            let drained = self.stream.feed(&buf[..n]);
            if !drained.lines.is_empty() {
                self.screen.insert_rows(drained.lines)?;
                self.dirty = true;
            }
            if !drained.passthrough.is_empty() {
                self.screen.tty.write_all(&drained.passthrough)?;
                self.screen.tty.flush()?;
            }
            if drained.partial_changed {
                self.screen.status = self.stream.partial().to_vec();
                self.dirty = true;
            }
        }
        Ok(any)
    }

    /// One bounded wait for a key, under the arbiter's stdin token. A modal
    /// prompt that owns stdin makes the token unavailable, in which case this
    /// just sleeps the idle interval so the loop keeps draining the pty.
    fn poll_keys(&mut self) -> io::Result<()> {
        let Some(_stdin) = newt_core::tty::try_watch_stdin() else {
            std::thread::sleep(IDLE_POLL);
            return Ok(());
        };
        // Wake on the pty too, so a burst of transcript never waits on the
        // keyboard poll.
        let mut fds = [
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.capture.master_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: poll on two descriptors we own.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, IDLE_POLL.as_millis() as i32) };
        if fds[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "terminal input closed",
            ));
        }
        // Cursor queries and earlier polls can leave parsed events in
        // crossterm after the kernel fd is empty. Check that queue before
        // waiting for another byte; keep the same exclusive stdin token.
        if (ready <= 0 || fds[0].revents & libc::POLLIN == 0) && !event::poll(Duration::ZERO)? {
            return Ok(());
        }
        // Drain every event crossterm has parsed so far.
        while event::poll(Duration::from_millis(1))? {
            let evt = event::read()?;
            // The modal takes the prompt token. Release the watcher token
            // first: acquiring a prompt while we still own it deadlocks.
            #[cfg(feature = "live-spill")]
            if matches!(evt, Event::Key(key) if key.kind == KeyEventKind::Press && key.code == crossterm::event::KeyCode::F(4))
            {
                drop(_stdin);
                return self.on_event(evt);
            }
            if let Event::Resize(cols, rows) = evt {
                // The terminal's own resize arrives here, and only here: measure where it
                // left the block rather than guess. Internal re-layouts keep the guess.
                self.dirty = true;
                let measured = self.measure_block_top();
                self.resize_to(cols, rows, measured)?;
                continue;
            }
            self.on_event(evt)?;
        }
        Ok(())
    }

    /// Apply a new terminal size. `measured_top` is where the terminal says the block now
    /// is (#3014); `None` keeps the arithmetic guess ([`resize_new_top`]).
    fn resize_to(&mut self, cols: u16, rows: u16, measured_top: Option<u16>) -> io::Result<()> {
        self.capture.resize(cols, rows);
        let status_rows = self.status_rows();
        let editor_rows = self.editor.wanted_rows(cols, rows, &self.surface.chrome());
        self.screen
            .resize(cols, rows, editor_rows, status_rows, measured_top)
    }

    /// Where the block's top is now, MEASURED: ask the terminal where its cursor is and
    /// subtract where the caret sits in the block. Terminals disagree on what a resize does
    /// to the screen (some pull scrollback down, some add rows below), so the old top is a
    /// guess; the cursor is the one thing every terminal answers (#3014). `None` when the
    /// answer cannot be trusted: a modal owns the cursor, the block moved since its last
    /// draw, or the terminal does not reply (then the guess stands, and the question is not
    /// asked again).
    fn measure_block_top(&mut self) -> Option<u16> {
        let screen = &self.screen;
        let frame = screen.caret_frame?;
        let current = CaretFrame {
            top: screen.top,
            block_h: screen.block_h,
            cols: screen.cols,
            rows: screen.rows,
            y: frame.y,
            x: frame.x,
        };
        if self.chat_inactive || !screen.can_query_cursor || frame != current {
            return None;
        }
        let offset = frame.y.checked_sub(frame.top)?;
        match self.capture.query_cursor() {
            Ok((_, row)) => block_top_from_reply(row, offset),
            Err(_) => {
                self.screen.can_query_cursor = false;
                None
            }
        }
    }

    fn on_event(&mut self, evt: Event) -> io::Result<()> {
        self.dirty = true;
        match evt {
            #[cfg(feature = "live-spill")]
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && key.code == crossterm::event::KeyCode::F(4) =>
            {
                if let Some(archive) = &self.spills {
                    let snapshot = archive.snapshot();
                    let output = self.screen.tty.try_clone()?;
                    let window = Self::suspend_terminal(self.screen.tty.try_clone()?);
                    let result = crate::transcript_pager::run_spill_picker(&snapshot, output);
                    drop(window);
                    let cleanup = self.finish_modal(None);
                    self.screen.term.clear()?;
                    self.draw()?;
                    cleanup?;
                    result?;
                }
                Ok(())
            }
            Event::Resize(cols, rows) => self.resize_to(cols, rows, None),
            // #2005: the ladder decides, from `assets/esc_ladder.toml`. This
            // arm replaced a hand-written Ctrl-C predicate; it is the SAME
            // interrupt, widened to Esc, not a second mechanism beside it.
            //
            // `KeyEventKind::Press` is load-bearing: without it, under the
            // kitty protocol the matching release event counts as a second
            // press and the operator's FIRST Ctrl-C is acknowledged as two.
            Event::Key(key) if key.kind == KeyEventKind::Press && self.escapes(&key) => {
                self.escape_during_turn();
                Ok(())
            }
            Event::Key(key) if key.kind == KeyEventKind::Press && self.meta_key(&key)? => Ok(()),
            other => {
                // #3022: `/spill open ` completes the ids retained so far. The
                // archive grows after the editor mounts, so refresh per key.
                #[cfg(feature = "live-spill")]
                if let Some(spills) = &self.spills {
                    self.editor.set_spill_choices(spills.choices());
                }
                let outcome = self.editor.on_event(other, &mut self.screen)?;
                if let Some(outcome) = outcome {
                    self.on_outcome(outcome);
                }
                Ok(())
            }
        }
    }

    /// The meta prefix at the prompt. `Ok(true)` when the key was the
    /// prefix's (consumed); `Ok(false)` hands it to the editor — including a
    /// doubled prefix, which is how the chord itself still reaches the editor.
    fn meta_key(&mut self, key: &crossterm::event::KeyEvent) -> io::Result<bool> {
        use crate::prefix::{MetaAction, Sequencer, Step, BINDINGS};
        let prefix = crate::prefix::current();
        if self.meta != Sequencer::new(prefix) && !self.meta.armed() {
            self.meta = Sequencer::new(prefix);
        }
        let ctrl = key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL);
        let note = match self
            .meta
            .feed(crate::panel::key_from_event(key.code, ctrl), &BINDINGS)
        {
            Step::Pass(_) => return Ok(false),
            Step::Armed | Step::Cancelled => return Ok(true),
            Step::Act(MetaAction::Redraw) => {
                self.screen.term.clear()?;
                self.draw()?;
                return Ok(true);
            }
            Step::Act(MetaAction::Help) => format!(
                "{}: {}  (zoom and resize act on an open panel)",
                crate::prefix::chord_label(prefix),
                BINDINGS.describe()
            ),
            Step::Act(action @ (MetaAction::Zoom | MetaAction::Resize)) => format!(
                "{} acts on an open panel (e.g. /settings) — at the prompt: {} then ? for keys",
                action.name(),
                crate::prefix::chord_label(prefix)
            ),
        };
        self.screen.insert_rows(vec![note.into_bytes()])?;
        Ok(true)
    }

    /// Rows for a blocking window `requested` high (#3056). One the operator opened with a
    /// command, outside any turn or operation it runs, draws at the bottom of the alternate
    /// screen, and leaving it gives the primary screen back exactly: nothing is scrolled,
    /// reclaimed or repainted. Anything else (a permission prompt during a turn or an
    /// operation, a clarification, a prompt at startup) reserves rows under the transcript,
    /// which stays in view (`plain_scroller_tui.md`).
    fn take_modal_rows(&mut self, requested: u16) -> io::Result<ModalRows> {
        let (reservation, alternate) = if self.serving_command && self.turn.is_none() {
            let out = crate::inline_viewport::PanelOut::Tty(self.screen.tty.try_clone()?);
            let screen = crate::transcript_pager::AltScreenGuard::enter_on(out, None)?;
            (None, Some(screen))
        } else {
            (Some(self.screen.reserve_modal_rows(requested)?), None)
        };
        Ok(ModalRows {
            requested,
            reservation,
            alternate,
        })
    }

    /// Where a blocking window draws: its inline reservation, else the bottom rows it asked
    /// for on the alternate screen, measured afresh. Afresh, because the presenter's geometry
    /// describes the primary screen and waits for it ([`Self::finish_modal_rows`]).
    fn lent_area(&self, rows: &ModalRows) -> Rect {
        if let Some(r) = &rows.reservation {
            return Rect::new(0, r.start, self.screen.cols, r.rows);
        }
        let (cols, height) = self
            .screen
            .terminal_size()
            .unwrap_or((self.screen.cols, self.screen.rows));
        let lent = rows.requested.min(height);
        Rect::new(0, height - lent, cols, lent)
    }

    /// #2571: the terminal resized under a live panel, or the operator sized it (Shift-↑/↓,
    /// zoom). The capture learns the new size and what the panel repaints is cleared: the
    /// alternate screen from its top, or an inline window's own rows, never the transcript
    /// above them. The presenter's own resize waits until the window closes
    /// ([`Self::finish_modal_rows`]).
    fn remeasure_panel(&mut self, rows: &ModalRows) -> io::Result<()> {
        let (cols, height) = self.screen.terminal_size()?;
        self.capture.resize(cols, height);
        let from = rows.reservation.as_ref().map_or(0, |r| r.start);
        self.screen.tty.write_all(&modal_cleanup_bytes(from)?)?;
        self.screen.tty.flush()
    }

    /// Give a blocking window's rows back and the keyboard to chat. The alternate screen, if
    /// the window had one, is left first, so what follows measures the primary screen. A
    /// dialog may have consumed every resize event while this presenter was parked, so the
    /// real tty is read before the draft is restored.
    fn finish_modal(&mut self, rows: Option<ModalRows>) -> io::Result<()> {
        self.chat_inactive = false;
        // Taking the reservation drops the rest, the alternate-screen guard included.
        let reservation = rows.and_then(|rows| rows.reservation);
        let cleanup = self.finish_modal_rows(reservation.as_ref());
        let released = self.screen.tty.release();
        cleanup.and(released)
    }

    fn finish_modal_rows(&mut self, reservation: Option<&ModalReservation>) -> io::Result<()> {
        let (cols, rows) = self.screen.terminal_size()?;
        if (cols, rows) == (self.screen.cols, self.screen.rows) {
            return reservation
                .map_or(Ok(()), |reservation| self.screen.release_modal(reservation));
        }
        let Some(reservation) = reservation else {
            // The terminal resized while the primary screen sat behind the alternate one:
            // ask where it left the block, as a resize at the prompt does (#3014).
            self.dirty = true;
            let measured = self.measure_block_top();
            return self.resize_to(cols, rows, measured);
        };
        // A narrower inline dialog may have expanded above its old top.
        self.screen.tty.write_all(&modal_cleanup_bytes(0)?)?;
        self.screen.tty.flush()?;
        // Reclaim before the resize re-anchors the block (it rebuilds the viewport).
        self.screen.reclaim_modal_rows(reservation);
        self.on_event(Event::Resize(cols, rows))
    }

    fn suspend_terminal(output: File) -> newt_core::tty::PromptWindow {
        newt_core::tty::Terminal::suspend_for_prompt_to(
            output,
            newt_core::tty::TerminalTaker::CockpitModal,
        )
    }

    fn run_bang_command(
        &mut self,
        command: &crate::OperatorCommand,
        color: bool,
        verbose: bool,
    ) -> io::Result<bool> {
        let (mut command, shell) = command.prepare()?;
        self.capture.foreground_stdio(&mut command)?;
        self.drain_pty()?;
        let result = (|| {
            let _window = Self::suspend_terminal(self.screen.tty.try_clone()?);
            let _cooked = self._raw.suspend()?;
            let mut modes_output = self.screen.tty.try_clone()?;
            let _modes = crate::RestoreOnDrop {
                restore: move || {
                    let _ = execute!(modes_output, crossterm::event::EnableBracketedPaste);
                },
            };
            self.screen.shutdown(&[])?;
            Ok(crate::run_bang_escape_unix(command, &shell, color, verbose))
        })();
        // Keep the child's output above the editor, even if the command
        // changed terminal dimensions. No cursor query or second input reader.
        let resumed = (|| {
            let (cols, rows) = self.screen.terminal_size()?;
            let height = (self.editor.wanted_rows(cols, rows, &self.surface.chrome())
                + self.status_rows())
            .clamp(1, rows);
            execute!(self.screen.tty, MoveTo(0, rows - 1))?;
            self.screen
                .tty
                .write_all(&vec![b'\n'; usize::from(height)])?;
            self.screen.top = rows - height;
            self.on_event(Event::Resize(cols, rows))?;
            self.draw()
        })();
        result.and_then(|success| resumed.map(|()| success))
    }

    /// Does this key press reach the operator's escape hatch right now?
    ///
    /// The whole precedence decision, in one readable predicate — codex's
    /// lesson (`bottom_pane/mod.rs:1310-1324`) with codex's own bug fixed. The
    /// conjuncts codex spells out by hand are rows in
    /// `assets/esc_ladder.toml`, and the claims come from accessors that live
    /// beside the state they read, so a new Esc consumer cannot be forgotten
    /// here.
    ///
    /// Two rungs are worth reading off the table rather than trusting prose:
    /// `ctrl-c` is RESERVED, so it escapes from every claim state while a turn
    /// runs and rungs 2–6 can never strand the operator; `esc` is
    /// FALLTHROUGH, so it escapes only once palette, `[y/N]`, `:`, INSERT and
    /// a pending operator have all declined.
    ///
    /// The permission modal is absent from the table because it is
    /// structurally unreachable from here, not because it was overlooked: on
    /// `SurfaceRequest::Interact` this presenter blocks INSIDE
    /// `handle_request` and never returns to `poll_keys` until the modal has
    /// answered.
    fn escapes(&self, key: &crossterm::event::KeyEvent) -> bool {
        let Some(trigger) = crate::esc_ladder::trigger_name(key) else {
            return false;
        };
        let claiming = self.editor.claim_set();
        matches!(
            crate::esc_ladder::ESC_LADDER.resolve(
                trigger,
                &precedence_ladder::Situation {
                    claiming: &claiming,
                    work_running: self.turn.is_some(),
                },
            ),
            precedence_ladder::Verdict::Escape { .. }
        )
    }

    /// Interrupt the running turn. The draft is kept; every press trips the
    /// same one-way `cancel` and is COUNTED for the spinner label, which
    /// acknowledges within a tick — the 1st as "interrupting…", the Nth as
    /// "×N heard — already stopping" (#2010). Same as the classic watcher.
    /// There is no second tier: the first press already drops the in-flight
    /// request and tool future, so a repeat has nothing left to force.
    ///
    /// **Private, with exactly one caller** (guard G1,
    /// `docs/decisions/key_ladder_crate.md` §5). Delete the ladder arm in
    /// `on_event` and this becomes dead code, which `cargo clippy -D warnings`
    /// fails on. That is a lint and not a theorem — making this `pub` or
    /// giving it a second caller voids it — so it is the cheap guard, not the
    /// primary one; `esc_ladder_pty_test` is the primary one.
    ///
    /// The press COUNTER lives in `newt_core::tty`, the spinner's owner,
    /// deliberately: it is what the label renders, both newt surfaces bump
    /// the same one, and `TurnEnded` is its one reset path — a copy here or
    /// inside the ladder crate would be a second count to keep in step.
    fn escape_during_turn(&mut self) {
        // Total rather than trusting the caller: `escapes` only returns true
        // while `work_running`, so this is unreachable, and an `unwrap` here
        // would be a panic waiting on a future refactor.
        let Some(turn) = self.turn.as_ref() else {
            return;
        };
        turn.cancel.store(true, Ordering::SeqCst);
        newt_core::tty::note_interrupt_press();
    }

    fn on_outcome(&mut self, outcome: EditorOutcome) {
        match outcome {
            EditorOutcome::Line(body) => self.submit(body),
            EditorOutcome::LineThenQuit(body) => {
                self.surface.arm_end_quit();
                self.submit(body);
            }
            EditorOutcome::EndAndQuit => {
                if let Some(reply) = self.pending_read.take() {
                    let _ = reply.send(Ok(ReadOutcome::EndAndQuit));
                } else {
                    self.surface.arm_end_quit();
                }
            }
            EditorOutcome::Tab(action) => {
                // Only the session can act on it, and only when it is
                // listening. Mid-turn tab motions wait for the persistent-
                // editor follow-up that lets a turn switch under a running
                // agent; here they are simply not taken.
                if let Some(reply) = self.pending_read.take() {
                    let _ = reply.send(Ok(ReadOutcome::Tab(action)));
                }
            }
            EditorOutcome::Eof => {
                if let Some(reply) = self.pending_read.take() {
                    let _ = reply.send(Ok(ReadOutcome::Eof));
                }
            }
        }
    }

    /// The session is about to serve `line`. It runs it as a command when it reads it as one,
    /// and then the windows it opens are the operator's (#3056).
    fn hand_over(&mut self, line: &str) {
        self.serving_command = crate::chat::operator_task(line).starts_with('/');
    }

    /// A submitted line: the session's if it is waiting for one, otherwise
    /// queued for the next `ReadLine`. It was already echoed into scrollback
    /// by the editor, so a queued line reads back exactly like a sent one.
    fn submit(&mut self, body: String) {
        if let Some(reply) = self.pending_read.take() {
            self.hand_over(&body);
            let _ = reply.send(Ok(ReadOutcome::Line(body)));
        } else {
            self.queued.push_back(body);
            self.screen.queued = self.queued.len();
        }
    }

    fn status_rows(&self) -> u16 {
        u16::from(!self.screen.status.is_empty() || !self.queued.is_empty())
    }

    /// The modal's raw-mode guard restores cooked mode when a `PromptWindow`
    /// closes; re-assert raw on that edge so keys keep arriving unbuffered.
    fn sync_modal_edge(&mut self) {
        let suspended = self.arbiter.suspended();
        if self.was_suspended != suspended {
            // NO RE-ASSERT ANY MORE (#1925). This used to call
            // `enable_raw_mode()` here, and its own comment said why it was
            // only belt and braces: "the modal restores the exact prior
            // termios itself now, but a stray `disable_raw_mode` anywhere
            // would otherwise leave us cooked."
            //
            // Both halves have since been closed. #1905 put every modal guard
            // on `RawModeGuard`, so a modal closing inside a raw cockpit hands
            // back RAW — what it found. And this file was the last production
            // member of the `raw-mode owners outside RawModeGuard` category, so
            // "a stray disable_raw_mode anywhere" cannot be added without
            // tripping the ratchet.
            //
            // A re-assert is also the one thing `RawModeGuard` deliberately
            // cannot express: it captures on construction and restores on
            // drop, and an `ensure_raw()` would capture the CURRENT mode as
            // "prior" — which, at the exact moment you would want to call it,
            // is the cooked mode you are trying to undo. The right answer was
            // to stop needing it, not to widen the type.
            // Repaint from nothing: whatever the modal (or the kernel's echo,
            // before that was fixed) put on our rows, ratatui's diff must not
            // be allowed to believe it is still ours.
            let _ = self.screen.term.clear();
            self.dirty = true;
        }
        self.was_suspended = suspended;
    }

    fn draw(&mut self) -> io::Result<()> {
        let status_rows = self.status_rows();
        let editor_rows =
            self.editor
                .wanted_rows(self.screen.cols, self.screen.rows, &self.surface.chrome());
        self.screen.relayout(editor_rows, status_rows)?;
        self.screen
            .draw(&self.editor, self.surface.chrome(), self.chat_inactive)?;
        self.dirty = false;
        self.last_draw = Instant::now();
        Ok(())
    }
}

/// Is the cockpit usable here? Rich surface already chosen by the caller;
/// this adds the pty preconditions: both stdio halves are terminals (the
/// same predicate `LineCaps` uses) and no protocol channel on fd 1.
pub(crate) fn supported() -> bool {
    use std::io::IsTerminal as _;
    io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && !newt_core::tty::protocol_mode()
        && std::env::var_os("NEWT_NO_COCKPIT").is_none()
}

#[cfg(test)]
mod tests {
    /// **The presenter USES the guard** (#1925), which the PTY tests cannot
    /// show. They prove `RawModeGuard` restores; a guard that is correct and
    /// unused is exactly the state these files were in before #1897.
    ///
    /// Counts CALL FORMS, never names: this file's doc comments discuss
    /// `enable_raw_mode()` and `disable_raw_mode` precisely because they
    /// explain why neither is called any more, and a name-based count would
    /// read its own explanation as a violation.
    #[test]
    fn the_cockpit_takes_raw_mode_only_through_the_guard() {
        let src = crate::production_source(include_str!("presenter.rs"));
        for call in [
            "enable_raw_mode()?",
            "enable_raw_mode();",
            "disable_raw_mode();",
        ] {
            assert_eq!(
                src.matches(call).count(),
                0,
                "`{call}` is a second raw-mode owner on crossterm's \
                 process-global; the cockpit takes raw through RawModeGuard"
            );
        }
        assert!(
            src.contains("_raw: newt_core::tty::raw_mode::RawModeGuard"),
            "the session must HOLD a RawModeGuard"
        );
    }

    /// The field order IS the restore order, and it is the half a reader can
    /// get wrong silently: fields drop in declaration order, so `_restore`
    /// (line wrap, bracketed paste, cursor) must come BEFORE `_raw`, or line
    /// discipline is handed back while paste markers are still armed (#1901).
    #[test]
    fn the_escape_restores_are_declared_before_raw_mode() {
        let src = crate::production_source(include_str!("presenter.rs"));
        let restore = src
            .find("    _restore: crate::RestoreOnDrop<fn()>,")
            .expect("the escape-sequence guard is a field");
        let raw = src
            .find("    _raw: newt_core::tty::raw_mode::RawModeGuard,")
            .expect("raw mode is a field");
        assert!(
            restore < raw,
            "_restore must be declared before _raw so the escape restores run \
             first; swapping them inverts the teardown order silently"
        );
    }

    use super::*;

    /// The geometry that has to be exactly right: where the block lands after
    /// `k` rows are written from `top` on a `rows`-high screen.
    #[test]
    fn insert_plan_below_the_fold_moves_the_block_down_without_scrolling() {
        // Screen 24 rows, block 4 rows at top=10, insert 3 rows.
        // Rows land at 10,11,12; block moves to 13; nothing scrolls.
        assert_eq!(
            plan_insert(10, 4, 24, 3),
            InsertPlan {
                extra_scroll: 0,
                new_top: 13
            }
        );
    }

    #[test]
    fn insert_plan_at_the_bottom_scrolls_by_exactly_the_rows_written() {
        // Block already at the bottom (top = 24-4 = 20). 3 rows written from
        // 20 land at 20,21,22 (no scroll yet); the block wants row 20 back,
        // so scroll 3 more.
        assert_eq!(
            plan_insert(20, 4, 24, 3),
            InsertPlan {
                extra_scroll: 3,
                new_top: 20
            }
        );
    }

    #[test]
    fn insert_plan_overshooting_the_screen_scrolls_the_overshoot_plus_the_block() {
        // top=20, block 4, screen 24, insert 10 rows: rows 20..23 fill the
        // screen, 6 more scroll as they are written (last row is 23), then
        // the block needs 4 rows → 4 more.
        assert_eq!(
            plan_insert(20, 4, 24, 10),
            InsertPlan {
                extra_scroll: 4,
                new_top: 20
            }
        );
    }

    #[test]
    fn insert_plan_crossing_the_fold_scrolls_only_what_does_not_fit() {
        // top=18, block 4, screen 24 (floor 20), insert 4: rows at 18..21,
        // block wants 22 but floor is 20 → scroll 2.
        assert_eq!(
            plan_insert(18, 4, 24, 4),
            InsertPlan {
                extra_scroll: 2,
                new_top: 20
            }
        );
    }

    #[test]
    fn modal_reservation_occludes_the_chat_block() {
        assert_eq!(
            plan_modal_reservation(20, 24, 5),
            ModalReservation {
                start: 19,
                rows: 5,
                chat_visible: false,
                transcript_end: 19,
            }
        );
        assert_eq!(
            plan_modal_reservation(3, 7, 8),
            ModalReservation {
                start: 0,
                rows: 7,
                chat_visible: false,
                transcript_end: 0,
            },
            "a short terminal gives the blocking modal the whole screen"
        );
        assert_eq!(
            plan_modal_reservation(0, 4, 2),
            ModalReservation {
                start: 2,
                rows: 2,
                chat_visible: false,
                transcript_end: 0,
            }
        );
    }

    /// #3014 mocked tier: the cursor the terminal reports locates the block it left behind
    /// (what the resize must erase), and a GROW pins the block to the new bottom whatever
    /// the terminal did (maintainer decision on item 2). Without a grow, the block's top
    /// follows the measured position.
    #[test]
    fn a_resize_follows_the_cursor_the_terminal_reports_not_a_guess() {
        // A 24-row screen, a 4-row block at the bottom (top 20) with the caret on its
        // second row (offset 1), grown to 34 rows.
        let (old_top, offset, old_rows, new_rows, new_h) = (20u16, 1u16, 24u16, 34u16, 4u16);
        let below = block_top_from_reply(old_top + offset, offset);
        assert_eq!(below, Some(20));
        let pulled = block_top_from_reply(old_top + offset + 10, offset);
        assert_eq!(pulled, Some(30));
        // A grow pins to the bottom (30) for a terminal that adds rows below, one that
        // pulls scrollback down, and one that never answers.
        for measured in [below, pulled, None] {
            assert_eq!(
                resized_top(measured, old_top, old_rows, new_rows, new_h),
                30
            );
        }
        // Not a grow: the measured position stands while it fits...
        assert_eq!(resized_top(Some(12), 20, 34, 34, 4), 12);
        // ...and a measured top that no longer fits the shrunk screen is clamped.
        assert_eq!(resized_top(Some(30), 20, 34, 24, 4), 20);
        // A reply above the caret's own offset cannot be this block's.
        assert_eq!(block_top_from_reply(0, 3), None);
    }

    /// A screen over a scratch file with a painted block, for driving `resize` and
    /// reading the bytes it wrote.
    fn screen_with_block(
        cols: u16,
        rows: u16,
        top: u16,
        block_h: u16,
        widths: &[usize],
        (caret_x, caret_y): (u16, u16),
    ) -> (Screen, std::fs::File) {
        let file = tempfile::tempfile().unwrap();
        let reader = file.try_clone().unwrap();
        let tty = WrapFence::new(file);
        let caret = CaretCell::new(std::sync::atomic::AtomicU32::new(NO_CARET));
        let backend = CaretBackend {
            inner: CrosstermBackend::new(tty.fenced_clone().unwrap()),
            caret: Arc::clone(&caret),
        };
        let term = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, top, cols, block_h)),
            },
        )
        .unwrap();
        let region = newt_core::tty::Terminal::lease_region(
            block_region(top, block_h),
            newt_core::tty::OnCollision::Refuse,
        )
        .expect("the test owns these rows");
        let screen = Screen {
            tty,
            term,
            caret,
            caret_frame: Some(CaretFrame {
                top,
                block_h,
                cols,
                rows,
                y: caret_y,
                x: caret_x,
            }),
            can_query_cursor: true,
            cols,
            rows,
            top,
            block_h,
            status_rows: 1,
            status: Vec::new(),
            queued: 0,
            region,
            painted_widths: widths.to_vec(),
        };
        (screen, reader)
    }

    /// The first row (zero-based) the resize erased downward from.
    fn first_erase_row(mut written: std::fs::File) -> u16 {
        use std::io::{Read as _, Seek as _, SeekFrom};
        let mut bytes = Vec::new();
        written.seek(SeekFrom::Start(0)).unwrap();
        written.read_to_end(&mut bytes).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        let at = text.find("\x1b[J").expect("resize erases downward");
        let (row, _) = text[..at]
            .rsplit_once("\x1b[")
            .and_then(|(_, m)| m.strip_suffix('H'))
            .and_then(|m| m.split_once(';'))
            .expect("a cursor move precedes the erase");
        row.parse::<u16>().unwrap() - 1
    }

    /// Every row (zero-based) the screen erased downward from, in order.
    fn erase_rows(mut written: std::fs::File) -> Vec<u16> {
        use std::io::{Read as _, Seek as _, SeekFrom};
        let mut bytes = Vec::new();
        written.seek(SeekFrom::Start(0)).unwrap();
        written.read_to_end(&mut bytes).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        // The last cursor move before each erase; the wrap fence's own escapes interleave.
        text.match_indices("\x1b[J")
            .map(|(at, _)| {
                let (row, _) = text[..at]
                    .rsplit("\x1b[")
                    .find_map(|m| m.strip_suffix('H')?.split_once(';'))
                    .expect("a cursor move precedes the erase");
                row.parse::<u16>().unwrap() - 1
            })
            .collect()
    }

    /// #3041: split (narrow), draw, close the split (widen), draw. A screen part-filled
    /// with transcript keeps the block where it is through both reflows, so no erase may
    /// start above row 10: rows 0..10 are transcript.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn a_narrow_then_widen_resize_never_erases_above_the_block() {
        let (mut screen, log) = screen_with_block(80, 24, 10, 3, &[80, 70, 10], (60, 11));
        // Narrow to 40: header and input each wrap once; the caret lands on row 13.
        screen.resize(40, 24, 2, 1, Some(13 - 1)).unwrap();
        assert_eq!(screen.top, 10, "the block did not move");
        // The redraw at 40 columns.
        screen.painted_widths = vec![40, 40, 10];
        screen.caret_frame = Some(CaretFrame {
            top: 10,
            block_h: 3,
            cols: 40,
            rows: 24,
            y: 11,
            x: 20,
        });
        // Widen back to 80: nothing in the block wraps, the caret is still on row 11.
        screen.resize(80, 24, 2, 1, Some(11 - 1)).unwrap();
        assert_eq!(screen.top, 10, "the block did not move");
        let erased = erase_rows(log);
        assert!(
            erased.iter().all(|&r| r >= 10),
            "transcript erased: erase rows {erased:?}"
        );
    }

    /// #3021 review: a width shrink whose footer wraps BELOW the caret. The terminal's
    /// reply already includes the scroll that reflow caused (block top 20), so erasing
    /// from `20 - growth` deleted the last transcript row above the block.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn a_measured_width_shrink_does_not_erase_transcript_above_the_block() {
        // 80x24, three-row block at row 21, caret on row 22; shrink to 40 wraps the
        // 70-column footer (+1 row), scrolling the block to row 20.
        let (mut screen, log) = screen_with_block(80, 24, 21, 3, &[10, 10, 70], (5, 22));
        screen.resize(40, 24, 2, 1, Some(20)).unwrap();
        assert_eq!(first_erase_row(log), 20, "row 19 is transcript");
    }

    /// The other side: a row ABOVE the caret wraps, pushing the caret down past the
    /// block's own offset, so the measured top reads one row too low.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn a_measured_width_shrink_with_a_wrapped_row_above_the_caret_erases_the_whole_block() {
        // Header (70 cols) wraps to two rows: the block now starts at 20 with the
        // caret on row 22, so `reply - offset` reads 21.
        let (mut screen, log) = screen_with_block(80, 24, 21, 3, &[70, 10, 10], (5, 22));
        screen.resize(40, 24, 2, 1, Some(21)).unwrap();
        assert_eq!(
            first_erase_row(log),
            20,
            "the wrapped header row is erased too"
        );
    }

    /// #3021 review, round 3: the caret's OWN row wraps before the caret. The input row
    /// (70 cols, caret at column 60) splits at 40, carrying the caret one row further down,
    /// so the reply reads one row lower than the header's wrap alone explains.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn a_measured_width_shrink_with_the_caret_row_wrapped_before_the_caret_erases_the_whole_block()
    {
        // Block at 10: header 80 cols (+1 row), input 70 cols with the caret at (60, 11).
        // At 40 cols the block stays at 10 and the caret lands on row 13.
        let (mut screen, log) = screen_with_block(80, 24, 10, 3, &[80, 70, 10], (60, 11));
        screen.resize(40, 24, 3, 1, Some(12)).unwrap();
        assert_eq!(first_erase_row(log), 10, "the old header row is erased too");
    }

    /// The control: the caret sits in the first fragment of its row, so only the header's
    /// wrap moved it.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn a_measured_width_shrink_with_the_caret_before_its_rows_wrap_point_erases_the_block() {
        let (mut screen, log) = screen_with_block(80, 24, 10, 3, &[80, 70, 10], (10, 11));
        screen.resize(40, 24, 3, 1, Some(11)).unwrap();
        assert_eq!(first_erase_row(log), 10);
    }

    /// The caret past its row's text, beyond the new width: terminals disagree on where
    /// reflow puts it, so the reply is not trusted and the unmeasured guess stands.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn a_caret_past_its_rows_text_beyond_the_new_width_falls_back_to_the_guess() {
        let (mut screen, log) = screen_with_block(80, 24, 10, 3, &[80, 50, 10], (60, 11));
        screen.resize(40, 24, 3, 1, Some(12)).unwrap();
        // Unmeasured: old top 10 lifted by the whole reflow (header +1, input +1).
        assert_eq!(first_erase_row(log), 8);
    }

    /// Unmeasured, the guess still subtracts the whole reflow.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn an_unmeasured_width_shrink_still_lifts_by_the_whole_reflow() {
        let (mut screen, log) = screen_with_block(80, 24, 21, 3, &[10, 10, 70], (5, 22));
        screen.resize(40, 24, 2, 1, None).unwrap();
        assert_eq!(first_erase_row(log), 20);
    }

    /// The screen after a transcript of `lines` rows, a block of `block_h` rows under it, then an
    /// inline modal of `requested` rows reserved, released, and one more transcript line
    /// inserted where the block now sits. Returns that screen.
    #[cfg(feature = "rich-tui")]
    fn panel_round_trip(rows: u16, lines: u16, block_h: u16, requested: u16) -> Vec<String> {
        let mut stream = String::new();
        for n in 0..lines {
            stream.push_str(&format!("\x1b[{};1HT{n:02}", n + 1));
        }
        let top = lines;
        for b in 0..block_h {
            stream.push_str(&format!("\x1b[{};1HBLOCK", top + b + 1));
        }
        let reservation = plan_modal_reservation(top, rows, requested);
        let bytes = modal_reserve_bytes(top, rows, &reservation).unwrap();
        stream.push_str(&String::from_utf8(bytes).unwrap());
        for r in reservation.start..rows {
            stream.push_str(&format!("\x1b[{};1HPANEL", r + 1));
        }
        stream
            .push_str(&String::from_utf8(modal_cleanup_bytes(reservation.start).unwrap()).unwrap());
        let new_top = reclaimed_top(&reservation, block_h, rows);
        let (insert, _) = render_insert(new_top, block_h, rows, &[b"NEXT".to_vec()]).unwrap();
        stream.push_str(&String::from_utf8(insert).unwrap());
        crate::interaction_view_pty_test::erasing_grid(&stream, usize::from(rows))
    }

    /// The row index of the first line starting `prefix`.
    #[cfg(feature = "rich-tui")]
    fn row_of(grid: &[String], prefix: &str) -> usize {
        grid.iter()
            .position(|row| row.starts_with(prefix))
            .unwrap_or_else(|| panic!("{prefix} is not on the screen: {grid:?}"))
    }

    /// #3012: a panel that covered transcript rows scrolled them into scrollback; releasing it
    /// moves the block back, so the next transcript line lands directly under the last one
    /// instead of below a permanent band of blank rows.
    #[cfg(feature = "rich-tui")]
    #[test]
    fn closing_a_panel_over_a_full_transcript_leaves_no_blank_rows() {
        // A 24-row screen, transcript T00..T20 above a 3-row block, a 10-row panel.
        let grid = panel_round_trip(24, 21, 3, 10);
        let (last, next) = (row_of(&grid, "T20"), row_of(&grid, "NEXT"));
        assert_eq!(next, last + 1, "NEXT must follow T20 directly: {grid:?}");
        assert!(
            !grid.iter().any(|row| row.contains("PANEL")),
            "panel text left behind: {grid:?}"
        );
    }

    /// A panel shorter than the block covers no transcript: nothing scrolled, and the block
    /// does not move.
    #[cfg(feature = "rich-tui")]
    #[test]
    fn closing_a_panel_that_covered_no_transcript_does_not_move_the_block() {
        let plan = plan_modal_reservation(10, 24, 4);
        assert_eq!(plan.transcript_end, 10);
        assert_eq!(reclaimed_top(&plan, 3, 24), 10);
        let grid = panel_round_trip(24, 10, 3, 4);
        assert_eq!(row_of(&grid, "NEXT"), row_of(&grid, "T09") + 1, "{grid:?}");
    }

    /// Control: this is what the release did before #3012: redraw the block at its old top.
    #[cfg(feature = "rich-tui")]
    #[test]
    fn the_old_release_left_the_gap_this_test_fails_to_hide() {
        let rows = 24u16;
        let plan = plan_modal_reservation(21, rows, 10);
        // Old behaviour: the block's top was never moved back.
        assert!(plan.start < 21, "the panel covered transcript rows");
        assert_eq!(reclaimed_top(&plan, 3, rows), plan.start);
        assert_ne!(
            reclaimed_top(&plan, 3, rows),
            21,
            "21 would leave blank rows"
        );
    }

    #[test]
    fn modal_cleanup_clears_outside_the_fixed_chat_viewport() {
        let cleanup = modal_cleanup_bytes(6).expect("cleanup bytes");
        let mut expected = Vec::new();
        queue!(expected, MoveTo(0, 6), Clear(ClearType::FromCursorDown)).expect("expected bytes");
        assert_eq!(cleanup, expected);
    }

    fn crlf_count(buf: &[u8]) -> usize {
        buf.windows(2).filter(|w| *w == b"\r\n").count()
    }

    /// #2 regression: with the block at the bottom, inserting `k` rows must emit
    /// exactly `k-1` `\r\n` separators and NO trailing line feed — the old
    /// per-row terminator scrolled the bottom row one extra time at
    /// `k == block_h` (and above), opening a blank gap over the block. Covers
    /// the boundary (`block_h`), one past it (`block_h + 1`), and a large burst.
    #[test]
    fn insert_at_the_bottom_emits_no_trailing_line_feed() {
        for k in [4usize, 5, 40] {
            let phys: Vec<Row> = (0..k).map(|i| format!("row {i}").into_bytes()).collect();
            let (buf, plan) = render_insert(20, 4, 24, &phys).unwrap();
            assert_eq!(
                crlf_count(&buf),
                k - 1,
                "k={k}: rows are separated by \\r\\n, never terminated by one"
            );
            assert!(
                !buf.ends_with(b"\r\n"),
                "k={k}: the buffer must not end on a line feed"
            );
            assert_eq!(plan.new_top, 20, "k={k}: the block stays bottom-anchored");
        }
    }

    /// Below the fold there is no scroll at all: the buffer ends on a style
    /// reset (the last row), not a line feed, and the block moves down by `k`.
    #[test]
    fn insert_below_the_fold_ends_on_a_reset_not_a_line_feed() {
        let phys: Vec<Row> = (0..3).map(|i| format!("r{i}").into_bytes()).collect();
        let (buf, plan) = render_insert(5, 4, 24, &phys).unwrap();
        assert_eq!(plan.extra_scroll, 0);
        assert_eq!(plan.new_top, 8);
        assert_eq!(crlf_count(&buf), 2);
        assert!(buf.ends_with(b"\x1b[0m"), "last row ends on a reset, no LF");
    }

    /// #1: the guard's restore sequence re-enables line wrap, disables bracketed
    /// paste, and shows the cursor — the output-side modes `open` took. The
    /// "runs on every exit path" property (the actual defect class) is proven by
    /// the crate's `splash_guard_tests`, which drive this same `RestoreOnDrop`;
    /// this pins the bytes the cockpit's guard emits. Asserted against a buffer
    /// because `io::stdout` is captured by the harness.
    /// Autowrap is off only between a write and its flush, so a resize that
    /// lands while the cockpit is idle finds wrap on and reflows the transcript.
    #[test]
    fn the_fence_turns_wrap_off_for_a_write_and_back_on_at_flush() {
        let mut fence = WrapFence::new(Vec::new());
        fence.write_all(b"a").unwrap();
        fence.write_all(b"b").unwrap();
        fence.flush().unwrap();
        fence.flush().unwrap();
        assert_eq!(fence.file, b"\x1b[?7lab\x1b[?7h");
    }

    /// #3011: a frame's writes and flushes, from any clone, reach the terminal
    /// as one synchronized write, so a compositor never paints a half frame
    /// (cursor hidden, caret not yet placed).
    #[test]
    fn a_frame_reaches_the_terminal_as_one_synchronized_write() {
        let mut screen = WrapFence::new(Vec::new());
        let mut backend = WrapFence {
            file: Vec::new(),
            state: Arc::clone(&screen.state),
        };
        screen.begin_frame();
        screen.write_all(b"\x1b[?25lstatus").unwrap();
        screen.flush().unwrap();
        backend.write_all(b"cells").unwrap();
        backend.flush().unwrap();
        backend.write_all(b"\x1b[?25h").unwrap();
        backend.flush().unwrap();
        assert!(
            screen.file.is_empty() && backend.file.is_empty(),
            "nothing sent mid-frame"
        );
        screen.end_frame().unwrap();
        assert_eq!(
            screen.file,
            b"\x1b[?2026h\x1b[?7l\x1b[?25lstatuscells\x1b[?25h\x1b[?2026l\x1b[?7h"
        );
        backend.write_all(b"after").unwrap();
        assert_eq!(
            backend.file, b"\x1b[?7lafter",
            "outside a frame, writes go straight through"
        );
    }

    /// Records each `write` as its own entry, so a test sees WRITE BOUNDARIES
    /// (a `Vec<u8>` keeps only the concatenation and cannot tell one write
    /// from a thousand), and each `flush` as a marker.
    #[derive(Clone, Default)]
    struct Boundaries(Arc<std::sync::Mutex<Vec<Option<Vec<u8>>>>>);

    impl Write for Boundaries {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().push(Some(buf.to_vec()));
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.lock().unwrap().push(None);
            Ok(())
        }
    }

    /// #3011: drives the PRODUCTION `Screen::draw` (status row, ratatui
    /// viewport, caret) and asserts the whole synchronized frame (`?2026h`
    /// through the caret's `Show`, to `?2026l`) crosses the writer as ONE
    /// write, with nothing flushed or written before it. Removing the
    /// `begin_frame`/`end_frame` wrapper, or splitting the payload into several
    /// writes, fails here; the fence unit test above cannot see either.
    #[test]
    fn a_production_draw_reaches_the_writer_as_one_synchronized_write() {
        let log = Boundaries::default();
        let tty = WrapFence::new(log.clone());
        let caret = CaretCell::new(std::sync::atomic::AtomicU32::new(NO_CARET));
        let backend = CaretBackend {
            inner: CrosstermBackend::new(WrapFence {
                file: log.clone(),
                state: Arc::clone(&tty.state),
            }),
            caret: Arc::clone(&caret),
        };
        let (cols, top, block_h) = (40, 2, 4);
        let term = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, top, cols, block_h)),
            },
        )
        .unwrap();
        let region = newt_core::tty::Terminal::lease_region(
            block_region(top, block_h),
            newt_core::tty::OnCollision::Refuse,
        )
        .expect("the test owns these rows");
        let mut screen = Screen {
            tty,
            term,
            caret,
            caret_frame: None,
            can_query_cursor: true,
            cols,
            rows: 10,
            top,
            block_h,
            status_rows: 1,
            status: b"working".to_vec(),
            queued: 0,
            region,
            painted_widths: Vec::new(),
        };
        let editor = MountedEditor::new(crate::rich_input::Edit::Emacs, None, Vec::new(), "hi");
        let chrome = crate::rich_input::Chrome {
            model: "m",
            endpoint: "e",
            gauge: None,
            session: "s",
            headline: "",
            modal: None,
            background_jobs: &[],
            tabs: &[],
            tools: "",
        };
        screen.draw(&editor, chrome, false).unwrap();

        let log = log.0.lock().unwrap();
        let writes: Vec<&Vec<u8>> = log.iter().flatten().collect();
        let frames: Vec<&&Vec<u8>> = writes
            .iter()
            .filter(|w| w.starts_with(SYNC_BEGIN))
            .collect();
        assert_eq!(frames.len(), 1, "exactly one synchronized frame: {log:?}");
        let frame = frames[0];
        assert!(
            frame.ends_with(SYNC_END),
            "the frame closes itself: {frame:?}"
        );
        let body = &frame[..frame.len() - SYNC_END.len()];
        let text = String::from_utf8_lossy(body);
        assert!(
            text.contains("working"),
            "status row in the frame: {text:?}"
        );
        assert!(text.contains("hi"), "viewport cells in the frame: {text:?}");
        assert!(
            text.contains("\x1b[?25l"),
            "status hides the cursor inside the frame: {text:?}"
        );
        // The caret sits after "❯ hi" on the viewport's first row (1-based
        // row 4, column 5): shown, then placed, last before SYNC_END.
        assert!(
            body.ends_with(b"\x1b[?25h\x1b[4;5H"),
            "show cursor and caret position close the frame: {text:?}"
        );
        let first = log.iter().position(|e| e.is_some()).unwrap();
        assert!(
            log[first].as_ref().unwrap().starts_with(SYNC_BEGIN),
            "nothing reaches the terminal before the frame: {log:?}"
        );
        // Everything after the frame is the single autowrap restore + flush.
        let end = log
            .iter()
            .position(|e| e.as_ref().is_some_and(|w| w.ends_with(SYNC_END)))
            .unwrap();
        assert!(
            log[end + 1..]
                .iter()
                .flatten()
                .all(|w| w.as_slice() == WRAP_ON),
            "only the autowrap restore follows the frame: {log:?}"
        );
    }

    #[test]
    fn a_held_fence_keeps_wrap_off_across_flushes_until_released() {
        let mut fence = WrapFence::new(Vec::new());
        fence.hold().unwrap();
        fence.write_all(b"modal").unwrap();
        fence.flush().unwrap();
        assert_eq!(fence.file, b"\x1b[?7lmodal");
        fence.release().unwrap();
        assert_eq!(fence.file, b"\x1b[?7lmodal\x1b[?7h");
    }

    /// The ratatui backend writes through its own clone. A hold taken on the
    /// screen's handle must bind it too, or a modal's `term.clear()` turns
    /// wrap back on underneath the dialog.
    #[test]
    fn a_hold_binds_every_clone_of_the_fence() {
        let mut screen = WrapFence::new(Vec::new());
        let mut backend = WrapFence {
            file: Vec::new(),
            state: Arc::clone(&screen.state),
        };
        screen.hold().unwrap();
        backend.write_all(b"clear").unwrap();
        backend.flush().unwrap();
        assert_eq!(backend.file, b"clear", "wrap is already off, and stays off");
        screen.release().unwrap();
        assert_eq!(screen.file, b"\x1b[?7l\x1b[?7h");
    }

    #[test]
    fn the_mode_restores_re_enable_wrap_disable_paste_and_show_the_cursor() {
        let mut buf = Vec::new();
        write_mode_restores(&mut buf).unwrap();
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("?7h"), "line wrap re-enabled: {s:?}");
        assert!(s.contains("?2004l"), "bracketed paste disabled: {s:?}");
        assert!(s.contains("?25h"), "cursor shown: {s:?}");
    }

    /// #4: `resize` clears from the higher of the old and new block tops, so the
    /// old cockpit region can't be stranded above a lower new block.
    #[test]
    fn a_narrower_terminal_lifts_the_old_block_by_its_wrapped_rows() {
        // 80-wide footer and 70-wide hint each take two rows at 47 columns.
        assert_eq!(reflow_growth(&[23, 70, 80], 47), 2);
        assert_eq!(
            reflow_growth(&[23, 70, 80], 120),
            0,
            "a wider terminal wraps nothing"
        );
        assert_eq!(
            reflow_growth(&[95], 47),
            2,
            "three rows for one 95-wide row"
        );
        assert_eq!(reflow_growth(&[47], 47), 0, "an exact fit does not wrap");
    }

    /// #3014 item 2 (maintainer decision, reversing #2441/#2632 for grows): a herdr pane
    /// grew from 30 to 63 rows with the block at top 27, height 3. The block is pinned to
    /// the new bottom row, `63 - 3 = 60`, not left under the content.
    #[test]
    fn growing_the_terminal_pins_the_block_to_the_new_bottom_3014() {
        assert_eq!(resize_new_top(27, 30, 63, 3), 60);
        // Pinned already: a second grow moves it again, and a same-size notice does not.
        assert_eq!(resize_new_top(60, 63, 90, 3), 87);
        assert_eq!(resize_new_top(60, 63, 63, 3), 60);
    }

    #[test]
    fn a_shrinking_terminal_still_bottom_anchors_the_block() {
        assert_eq!(resize_new_top(20, 24, 10, 3), 7);
    }

    #[test]
    fn a_grow_by_one_row_pins_the_block_too_3014() {
        assert_eq!(resize_new_top(27, 30, 31, 5), 26);
    }

    /// #2632 review finding 1, kept: a same-size notification (no rows added or removed)
    /// must not move a block, so it cannot recreate a gap with no terminal movement.
    #[test]
    fn repeated_same_size_resizes_do_not_move_the_block_2632() {
        assert_eq!(resize_new_top(20, 24, 24, 4), 20);
        assert_eq!(
            resize_new_top(15, 24, 24, 4),
            15,
            "off the bottom, still fits"
        );
    }

    /// #2632 review finding 1, kept: a height-only shrink that still leaves the block room
    /// must not re-anchor it.
    #[test]
    fn a_partial_shrink_that_still_fits_keeps_the_old_position_2632() {
        assert_eq!(resize_new_top(27, 63, 40, 3), 27);
    }

    #[test]
    fn resize_erases_from_the_higher_of_the_old_and_new_block_tops() {
        // Terminal grew 24->30, block 4: old top 20, new top 26 — clear from 20.
        assert_eq!(resize_erase_from(20, 26, 30), 20);
        // Block grew taller on the same screen: old top 20, new top 12.
        assert_eq!(resize_erase_from(20, 12, 24), 12);
        // Screen shrank 24->10: the old top is off-screen; clamp to the new top.
        assert_eq!(resize_erase_from(20, 6, 10), 6);
        // Unchanged geometry clears from the shared top.
        assert_eq!(resize_erase_from(20, 20, 24), 20);
    }

    #[test]
    fn a_styled_line_round_trips_to_ansi_with_a_reset_per_span() {
        use ratatui::style::{Color, Style};
        use ratatui::text::Span;
        let line = Line::from(vec![
            Span::styled("[t]", Style::default().fg(Color::DarkGray)),
            Span::styled(
                " body",
                Style::default().fg(Color::White).bg(Color::Rgb(82, 82, 82)),
            ),
        ]);
        let bytes = line_to_ansi(&line).unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("[t]"), "{s:?}");
        assert!(s.contains(" body"), "{s:?}");
        let color_suppressed = std::env::var_os("NO_COLOR").is_some()
            || std::env::var("TERM").as_deref() == Ok("dumb");
        assert_eq!(
            s.contains("48;2;82;82;82"),
            !color_suppressed,
            "command background follows the runtime color policy: {s:?}"
        );
        assert!(s.contains("\x1b[0m"), "reset present: {s:?}");
        assert_eq!(super::super::ansi::visible_width(&bytes), "[t] body".len());
    }
}

/// Real-terminal acceptance for the cockpit's ownership of the operator's
/// terminal (#1744), against a pty the test owns — never the developer's.
///
/// **One cockpit per process.** A completed `Presenter` lifecycle leaves
/// process-global terminal state behind (crossterm resolves and caches it), so
/// a second `Presenter::open` in the same test binary times out waiting for its
/// cursor report. The real session opens exactly one cockpit, so this is a
/// property of the harness rather than of the product — but it means the
/// behaviours have to be proven by ONE cockpit, in sequence, which is what the
/// single test below does. The panic path is proven separately against the
/// modes guard itself, which is the mechanism that makes the guarantee.
#[cfg(test)]
#[path = "presenter_terminal_acceptance.rs"]
mod terminal_acceptance;

#[cfg(all(test, feature = "live-spill"))]
pub(crate) use terminal_acceptance::cockpit_pager_case;
#[cfg(all(test, feature = "rich-tui"))]
pub(crate) use terminal_acceptance::cockpit_psyche_case;
#[cfg(test)]
pub(crate) use terminal_acceptance::{
    cockpit_acceptance_case, cockpit_bang_case, cockpit_buffered_input_case,
    cockpit_clarification_input_case, cockpit_panel_gap_case, cockpit_panel_loop_case,
    cockpit_panel_reopen_case, cockpit_resize_anchor_case, panel_live_resize_case,
    panel_resize_case,
};

#[cfg(test)]
#[path = "presenter_migration_acceptance.rs"]
mod migration_acceptance;
#[cfg(test)]
pub(crate) use migration_acceptance::{
    cockpit_migration_case, persona_migration_case, startup_migration_case,
};
