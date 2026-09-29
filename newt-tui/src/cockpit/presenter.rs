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
//! **No cursor queries after start.** Once fd 1 is on the pty, `ESC[6n` would
//! go to the pty and its answer would never come — so the viewport is
//! [`Viewport::Fixed`], placed by arithmetic, and the one position query
//! happens in [`Presenter::open`] before the capture is installed. Two
//! consequences follow and are enforced here rather than remembered:
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
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
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
}

const WRAP_OFF: &[u8] = b"\x1b[?7l";
const WRAP_ON: &[u8] = b"\x1b[?7h";

impl<W: Write> WrapFence<W> {
    fn new(file: W) -> Self {
        Self {
            file,
            state: Arc::default(),
        }
    }

    fn turn_off(&mut self) -> io::Result<()> {
        if !self.state.wrap_off.swap(true, Ordering::Relaxed) {
            self.file.write_all(WRAP_OFF)?;
        }
        Ok(())
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
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
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

/// The screen: the real terminal, and where the block is on it.
///
/// Split from [`Presenter`] so the editor can be handed `&mut Screen` as its
/// [`ScrollbackSink`] while the presenter still borrows the editor.
struct Screen {
    tty: WrapFence,
    term: Terminal<CrosstermBackend<WrapFence>>,
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

#[derive(Debug, PartialEq, Eq)]
struct ModalReservation {
    start: u16,
    rows: u16,
    chat_visible: bool,
}

fn plan_modal_reservation(_block_top: u16, screen_rows: u16, requested: u16) -> ModalReservation {
    let rows = requested.min(screen_rows);
    ModalReservation {
        start: screen_rows.saturating_sub(rows),
        rows,
        chat_visible: false,
    }
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

    fn rebuild_term(&mut self) -> io::Result<()> {
        let backend = CrosstermBackend::new(self.tty.fenced_clone()?);
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
    /// Re-reserve a live panel's rows after a resize (#2573). Unlike
    /// [`Self::reserve_modal_rows`] this erases only from `erase_from` — what
    /// the OLD panel could still occupy — and scrolls nothing: the transcript
    /// above the panel is left where the terminal put it.
    fn remeasure_modal_rows(
        &mut self,
        requested: u16,
        erase_from: u16,
    ) -> io::Result<ModalReservation> {
        let plan = plan_modal_reservation(self.top, self.rows, requested);
        self.tty.hold()?;
        let mut buf = Vec::new();
        queue!(
            buf,
            crossterm::cursor::Hide,
            MoveTo(0, erase_from.min(plan.start)),
            Clear(ClearType::FromCursorDown)
        )?;
        self.tty.write_all(&buf)?;
        self.tty.flush()?;
        self.term.clear()?;
        Ok(plan)
    }

    fn reserve_modal_rows(&mut self, requested: u16) -> io::Result<ModalReservation> {
        self.reserve_modal_rows_from(self.top, requested)
    }

    /// Reserve `requested` rows for a modal whose rows today start at
    /// `occupied_top` — the block's top when a modal opens, the panel's own top
    /// when the operator sizes it (#2574). Only transcript rows the new plan
    /// covers above `occupied_top` scroll into scrollback.
    fn reserve_modal_rows_from(
        &mut self,
        occupied_top: u16,
        requested: u16,
    ) -> io::Result<ModalReservation> {
        let plan = plan_modal_reservation(self.top, self.rows, requested);
        self.tty.hold()?;
        let mut buf = Vec::new();
        queue!(
            buf,
            crossterm::cursor::Hide,
            MoveTo(0, occupied_top),
            Clear(ClearType::FromCursorDown)
        )?;
        let covered_transcript = occupied_top.saturating_sub(plan.start);
        if covered_transcript > 0 {
            queue!(buf, MoveTo(0, self.rows.saturating_sub(1)))?;
            buf.extend(std::iter::repeat_n(b'\n', covered_transcript as usize));
        }
        queue!(buf, MoveTo(0, plan.start), Clear(ClearType::FromCursorDown))?;
        self.tty.write_all(&buf)?;
        self.tty.flush()?;
        self.term.clear()?;
        Ok(plan)
    }

    fn place_cursor(&mut self, row: u16) -> io::Result<()> {
        execute!(self.tty, MoveTo(0, row))?;
        self.tty.flush()
    }

    fn cleanup_modal(&mut self, reservation: &ModalReservation) -> io::Result<()> {
        if reservation.chat_visible {
            return Ok(());
        }
        self.tty
            .write_all(&modal_cleanup_bytes(reservation.start)?)?;
        self.tty.flush()
    }

    /// Insert finished rows into the transcript above the block.
    ///
    /// The byte plan is [`render_insert`] (pure, testable); this writes it and
    /// re-seats the block viewport at its new top. All arithmetic, no queries —
    /// see the module docs.
    fn insert_rows(&mut self, rows: Vec<Row>) -> io::Result<()> {
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

    /// The terminal changed size: put the block at the bottom of the new
    /// screen and start the viewport clean. The transcript above is the
    /// terminal's to reflow.
    fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        editor_rows: u16,
        status_rows: u16,
    ) -> io::Result<()> {
        let old_top = self.top;
        // Narrowing reflows any old block row wider than the new terminal onto
        // extra rows, lifting the stale block above `old_top`.
        let lifted = if cols.max(1) < self.cols {
            reflow_growth(&self.painted_widths, cols.max(1))
        } else {
            0
        };
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        let new_h = (editor_rows + status_rows).clamp(1, self.rows);
        self.block_h = new_h;
        self.status_rows = status_rows;
        self.top = self.rows - new_h;
        // FORCED: the TERMINAL resized; the block's new position is a
        // consequence, not a request.
        self.region.relocate(
            block_region(self.top, self.block_h),
            newt_core::tty::OnCollision::SuspendHolder,
        );
        // Erase from the topmost of the OLD and new block tops, not just the new
        // one (#4), raised by the rows the old block reflowed onto. Both blocks
        // are bottom-anchored, so this wipes every stale status/editor row;
        // nothing but block chrome sits below that row, so no transcript is
        // lost.
        let erase_from = resize_erase_from(old_top, self.top, self.rows).saturating_sub(lifted);
        let mut buf = Vec::new();
        queue!(buf, MoveTo(0, erase_from), Clear(ClearType::FromCursorDown))?;
        self.tty.write_all(&buf)?;
        self.tty.flush()?;
        self.rebuild_term()
    }

    /// Paint the block: the status row (raw, clipped, with the queued chip at
    /// the right edge), then the ratatui viewport.
    fn draw(
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
        let frame = self.term.draw(|f| editor.draw(f, chrome, chat_inactive))?;
        let area = frame.area;
        widths.extend((area.top()..area.bottom()).map(|y| {
            (area.left()..area.right())
                .rev()
                .find(|&x| frame.buffer[(x, y)].symbol() != " ")
                .map_or(0, |x| usize::from(x - area.left()) + 1)
        }));
        self.painted_widths = widths;
        Ok(())
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

/// The row `resize` clears downward from so the OLD cockpit region cannot
/// survive above the new block (#4). Both blocks are bottom-anchored, so the
/// topmost of the two tops covers both regions; clamped into the (possibly
/// smaller) new screen.
fn resize_erase_from(old_top: u16, new_top: u16, rows: u16) -> u16 {
    old_top.min(new_top).min(rows.saturating_sub(1))
}

/// The row a live panel's resize erases from (#2573): the higher of its old
/// and new tops — both bottom-anchored, the same rule the block uses — raised
/// by the rows a narrower terminal reflowed its old full-width rows onto.
/// Nothing above that is the panel's, so the transcript survives.
fn panel_erase_from(
    old_start: u16,
    old_rows: u16,
    old_cols: u16,
    cols: u16,
    rows: u16,
    requested: u16,
) -> u16 {
    let new_start = plan_modal_reservation(0, rows, requested).start;
    let lifted = if cols < old_cols {
        reflow_growth(&vec![usize::from(old_cols); usize::from(old_rows)], cols)
    } else {
        0
    };
    resize_erase_from(old_start, new_start, rows).saturating_sub(lifted)
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
        let backend = CrosstermBackend::new(tty.fenced_clone()?);
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
                // A confirmed `:wq` submitted its turn last time; now that the
                // turn has run, end the conversation and exit before reading
                // anything new — same order as the classic surface.
                if self.surface.take_end_quit() {
                    let _ = reply.send(Ok(ReadOutcome::EndAndQuit));
                } else if let Some(line) = self.queued.pop_front() {
                    self.screen.queued = self.queued.len();
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
                let reservation = self.screen.reserve_modal_rows(requested_rows)?;
                self.chat_inactive = true;
                if reservation.chat_visible {
                    if let Err(error) = self.draw() {
                        self.chat_inactive = false;
                        return Err(error);
                    }
                }
                if let Err(error) = self.screen.place_cursor(reservation.start) {
                    self.chat_inactive = false;
                    let _ = self.finish_modal(Some(&reservation));
                    let _ = self.draw();
                    return Err(error);
                }
                let window = Self::suspend_terminal(prompt_output);
                let result: io::Result<ReadOutcome> = self
                    .screen
                    .tty
                    .try_clone()
                    .and_then(|out| {
                        crate::inline_viewport::cockpit_panel_terminal(
                            out,
                            Rect::new(0, reservation.start, self.screen.cols, reservation.rows),
                        )
                    })
                    .and_then(|mut terminal| {
                        crate::clarification_modal::present_in(&mut terminal, &batch, &hint)
                    });
                drop(window);
                let modal_cleanup = self.finish_modal(Some(&reservation));
                self.chat_inactive = false;
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
            #[cfg(feature = "live-spill")]
            SurfaceRequest::SetSpillArchive(archive) => {
                archive.enable_inspection();
                self.spills = Some(archive);
                self.screen.insert_rows(vec![
                    b"F4 inspect retained output (including /spill open IDs)".to_vec(),
                ])?;
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
                let reservation = self.screen.reserve_modal_rows(requested_rows)?;
                // The modal is the only visible input surface until it exits.
                self.chat_inactive = true;
                if reservation.chat_visible {
                    if let Err(error) = self.draw() {
                        self.chat_inactive = false;
                        return Err(error);
                    }
                }
                if let Err(error) = self.screen.place_cursor(reservation.start) {
                    self.chat_inactive = false;
                    let _ = self.finish_modal(Some(&reservation));
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
                    let terminal = crate::inline_viewport::cockpit_panel_terminal(
                        out,
                        Rect::new(0, reservation.start, self.screen.cols, reservation.rows),
                    )?;
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
                let modal_cleanup = self.finish_modal(Some(&reservation));
                self.chat_inactive = false;
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
            // park while the session draws. Inline panels reserve rows;
            // alternate-screen panels preserve the primary buffer themselves.
            // Both use the same focus transfer and release channel.
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
                let reservation = match mode {
                    PanelMode::Inline(rows) => Some(self.screen.reserve_modal_rows(rows)?),
                    // The caller's alternate buffer occludes the composer and
                    // saves the primary screen. Do not erase or scroll it first.
                    PanelMode::AlternateScreen => None,
                };
                self.chat_inactive = true;
                if reservation
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
                    self.lent_area(reservation.as_ref()),
                    Some(release),
                );
                if reply.send(Some(window)).is_err() {
                    // The session vanished between asking and receiving. Undo
                    // the reservation rather than parking forever.
                    self.chat_inactive = false;
                    let cleanup = self.finish_modal(reservation.as_ref());
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
                let mut reservation = reservation;
                let mut mode = mode;
                while let Ok(crate::session_worker::PanelSignal::Remeasure { rows, reply }) =
                    released.recv()
                {
                    // The operator sized the panel (Shift-↑/↓, zoom): an inline
                    // loan takes the new request; the screen clamps it.
                    if let (Some(rows), PanelMode::Inline(_)) = (rows, mode) {
                        mode = PanelMode::Inline(rows);
                    }
                    // A failed re-plan keeps the old region rather than
                    // stranding the panel: the reply still goes out.
                    let _ = self.remeasure_panel(mode, &mut reservation);
                    let _ = reply.send(self.lent_area(reservation.as_ref()));
                }
                let modal_cleanup = self.finish_modal(reservation.as_ref());
                self.chat_inactive = false;
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
            self.on_event(evt)?;
        }
        Ok(())
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
            Event::Resize(cols, rows) => {
                self.capture.resize(cols, rows);
                let status_rows = self.status_rows();
                let editor_rows = self.editor.wanted_rows(cols, rows, &self.surface.chrome());
                self.screen.resize(cols, rows, editor_rows, status_rows)
            }
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

    /// The region lent to a panel: its reservation, or the whole screen for an
    /// alternate-screen loan.
    fn lent_area(&self, reservation: Option<&ModalReservation>) -> Rect {
        let (top, rows) = reservation.map_or((0, self.screen.rows), |r| (r.start, r.rows));
        Rect::new(0, top, self.screen.cols, rows)
    }

    /// #2571: the terminal resized under a live panel, or the operator asked
    /// for a different height (Shift-↑/↓, zoom). The same re-layout
    /// `finish_modal_rows` applies to a resize that lands after a dialog
    /// closes — clear what a narrower panel may have rewrapped above its old
    /// top, then the presenter's own resize — and the panel's rows reserved
    /// again against the new screen.
    fn remeasure_panel(
        &mut self,
        mode: PanelMode,
        reservation: &mut Option<ModalReservation>,
    ) -> io::Result<()> {
        let (cols, rows) = self.screen.terminal_size()?;
        let resized = (cols, rows) != (self.screen.cols, self.screen.rows);
        // Measured before the presenter's own resize moves its geometry.
        let old = reservation.as_ref().map(|r| (r.start, r.rows));
        let old_cols = self.screen.cols;
        if resized {
            self.on_event(Event::Resize(cols, rows))?;
        }
        // An alternate-screen loan owns the whole screen and redraws it: the
        // presenter's own resize above is its whole resize contract (#2573).
        let (PanelMode::Inline(requested), Some((old_start, old_rows))) = (mode, old) else {
            return Ok(());
        };
        if resized {
            let erase_from = panel_erase_from(old_start, old_rows, old_cols, cols, rows, requested);
            *reservation = Some(self.screen.remeasure_modal_rows(requested, erase_from)?);
        } else if old_rows != requested.min(self.screen.rows) {
            // The operator sized the panel (#2574): reserved from the panel's
            // own top, so a grow scrolls only the rows it newly covers into
            // scrollback and a shrink erases only the rows it gives back.
            *reservation = Some(self.screen.reserve_modal_rows_from(old_start, requested)?);
        }
        Ok(())
    }

    /// A blocking dialog may have consumed every resize event while this
    /// presenter was parked. Read the real tty before restoring its draft.
    fn finish_modal(&mut self, reservation: Option<&ModalReservation>) -> io::Result<()> {
        let cleanup = self.finish_modal_rows(reservation);
        let released = self.screen.tty.release();
        cleanup.and(released)
    }

    fn finish_modal_rows(&mut self, reservation: Option<&ModalReservation>) -> io::Result<()> {
        let (cols, rows) = self.screen.terminal_size()?;
        if (cols, rows) != (self.screen.cols, self.screen.rows) {
            // A narrower inline dialog may have expanded above its old top.
            // Alternate-screen loans never painted the primary buffer; only
            // inline dialogs need their old pixels cleared before resizing.
            if reservation.is_some() {
                self.screen.tty.write_all(&modal_cleanup_bytes(0)?)?;
                self.screen.tty.flush()?;
            }
            self.on_event(Event::Resize(cols, rows))
        } else if let Some(reservation) = reservation {
            self.screen.cleanup_modal(reservation)
        } else {
            Ok(())
        }
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

    /// A submitted line: the session's if it is waiting for one, otherwise
    /// queued for the next `ReadLine`. It was already echoed into scrollback
    /// by the editor, so a queued line reads back exactly like a sent one.
    fn submit(&mut self, body: String) {
        if let Some(reply) = self.pending_read.take() {
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
            }
        );
        assert_eq!(
            plan_modal_reservation(3, 7, 8),
            ModalReservation {
                start: 0,
                rows: 7,
                chat_visible: false,
            },
            "a short terminal gives the blocking modal the whole screen"
        );
        assert_eq!(
            plan_modal_reservation(0, 4, 2),
            ModalReservation {
                start: 2,
                rows: 2,
                chat_visible: false,
            }
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

    /// #2573 review: a live panel's resize erases only rows the panel could
    /// still occupy. The regression: clearing from row 0 wiped the transcript.
    #[test]
    fn a_panel_resize_erases_only_what_the_old_panel_could_occupy() {
        // Same height, wider: only the panel's own rows (13..).
        assert_eq!(panel_erase_from(13, 18, 100, 118, 31, 18), 13);
        // Taller terminal: the old panel at 13.. is stale; nothing above it.
        assert_eq!(panel_erase_from(13, 18, 100, 100, 45, 18), 13);
        // Shorter: the new panel starts higher, so erase from its top.
        assert_eq!(panel_erase_from(13, 18, 100, 100, 25, 18), 7);
        // Narrower: each full-width old row reflows onto two, lifting the
        // stale panel by its height — never past the top of the screen.
        assert_eq!(panel_erase_from(20, 8, 100, 60, 31, 8), 12);
        assert_eq!(panel_erase_from(13, 18, 100, 60, 31, 18), 0);
        // Never row 0 unless the reflow truly reaches it.
        assert!(panel_erase_from(20, 8, 100, 118, 31, 8) > 0);
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
#[cfg(test)]
pub(crate) use terminal_acceptance::{
    cockpit_acceptance_case, cockpit_bang_case, cockpit_buffered_input_case,
    cockpit_clarification_input_case, cockpit_panel_loop_case, panel_live_resize_case,
    panel_resize_case,
};

#[cfg(test)]
#[path = "presenter_migration_acceptance.rs"]
mod migration_acceptance;
#[cfg(test)]
pub(crate) use migration_acceptance::{
    cockpit_migration_case, persona_migration_case, startup_migration_case,
};
