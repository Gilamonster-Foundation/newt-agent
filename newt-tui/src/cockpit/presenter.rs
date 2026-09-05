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
use std::io::{self, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::style::{
    Attribute, Color as CColor, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{Clear, ClearType, DisableLineWrap, EnableLineWrap};
use crossterm::{execute, queue};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::{Terminal, TerminalOptions, Viewport};

use super::ansi::{clip_to_width, wrap_row, Row, TranscriptStream};
use super::pty::PtyCapture;
use crate::rich_input::{Chrome, EditorOutcome, MountedEditor, RichSurface, ScrollbackSink};
use crate::session_worker::SurfaceRequest;
use crate::{InputSurface, ReadOutcome};

/// How long the loop sleeps in `poll` when nothing is happening. Bounds the
/// latency of a transcript byte, a keystroke, and a surface request alike.
const IDLE_POLL: Duration = Duration::from_millis(20);
/// The live clock in the header ticks at this cadence when idle (as before).
const CLOCK_TICK: Duration = Duration::from_millis(250);
/// After the session drops its end, keep draining the pty until it has been
/// quiet this long — the last lines it printed may still be in flight.
const DRAIN_QUIET: Duration = Duration::from_millis(120);

/// The screen: the real terminal, and where the block is on it.
///
/// Split from [`Presenter`] so the editor can be handed `&mut Screen` as its
/// [`ScrollbackSink`] while the presenter still borrows the editor.
struct Screen {
    tty: File,
    term: Terminal<CrosstermBackend<File>>,
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

fn plan_modal_reservation(block_top: u16, screen_rows: u16, requested: u16) -> ModalReservation {
    if requested <= block_top {
        ModalReservation {
            start: block_top - requested,
            rows: requested,
            chat_visible: true,
        }
    } else {
        // The dialog and chat block cannot both fit. Give the blocking surface
        // the whole screen; hiding the inactive chat box is preferable to
        // letting the dialog scroll through a still-live ratatui viewport.
        ModalReservation {
            start: 0,
            rows: screen_rows,
            chat_visible: false,
        }
    }
}

/// Physical terminal rows occupied by the canonical plain modal body.
///
/// The cockpit normally disables terminal autowrap because all of its own
/// rows are pre-wrapped. The width-aware core terminal adapter applies this
/// same shared wrapper to the canonical plain body while leaving the editable
/// answer row in no-wrap mode.
fn modal_physical_rows(body: &str, cols: u16) -> usize {
    newt_core::tty::wrap_line(body, usize::from(cols.max(1))).len()
}

fn modal_requested_rows(body: &str, cols: u16) -> u16 {
    u16::try_from(modal_physical_rows(body, cols).saturating_add(1)).unwrap_or(u16::MAX)
}

/// Bytes needed only when a constrained terminal gave the modal the whole
/// visible screen. Ratatui's fixed-viewport `clear()` deliberately touches
/// only the chat block, so it cannot remove modal rows outside that viewport.
fn full_screen_modal_cleanup() -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    queue!(buf, MoveTo(0, 0), Clear(ClearType::All))?;
    Ok(buf)
}

impl Screen {
    fn viewport_rect(&self) -> Rect {
        Rect::new(
            0,
            self.top + self.status_rows,
            self.cols,
            self.block_h - self.status_rows,
        )
    }

    fn rebuild_term(&mut self) -> io::Result<()> {
        let backend = CrosstermBackend::new(self.tty.try_clone()?);
        self.term = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(self.viewport_rect()),
            },
        )?;
        self.term.clear()
    }

    /// Open clean rows immediately above the persistent cockpit block.
    ///
    /// Scrolling first preserves the transcript in the terminal's history;
    /// clearing only the shifted copy of the ephemeral block then leaves the
    /// requested modal rows blank. The chat block is repainted in place below
    /// them, so the operator can see its dimmed prompt and the modal's active
    /// prompt at the same time.
    fn reserve_modal_rows(&mut self, requested: u16) -> io::Result<ModalReservation> {
        let plan = plan_modal_reservation(self.top, self.rows, requested);
        if !plan.chat_visible {
            let mut buf = Vec::new();
            // Remove the ephemeral block before preserving the visible
            // transcript in scrollback. That keeps a duplicate editor out of
            // history while opening a full-screen fallback for the modal.
            queue!(buf, MoveTo(0, self.top), Clear(ClearType::FromCursorDown))?;
            if self.top > 0 {
                queue!(buf, MoveTo(0, self.rows.saturating_sub(1)))?;
                buf.extend(std::iter::repeat_n(b'\n', self.top as usize));
            }
            queue!(buf, MoveTo(0, 0), Clear(ClearType::FromCursorDown))?;
            self.tty.write_all(&buf)?;
            self.tty.flush()?;
            self.term.clear()?;
            return Ok(plan);
        }
        if plan.rows == 0 {
            return Ok(plan);
        }
        let mut buf = Vec::new();
        queue!(buf, MoveTo(0, self.rows.saturating_sub(1)))?;
        buf.extend(std::iter::repeat_n(b'\n', plan.rows as usize));
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
        self.tty.write_all(&full_screen_modal_cleanup()?)?;
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
        // one (#4). Both blocks are bottom-anchored — each occupies
        // `[top, rows)` — so clearing from `min(old_top, new_top)` down wipes the
        // old cockpit region too. Without it a grown terminal (new top below the
        // old) leaves the previous status/editor/tab-bar rows stranded above the
        // new block. Nothing but block chrome ever sits below that row, so no
        // transcript is lost.
        let erase_from = resize_erase_from(old_top, self.top, self.rows);
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
        self.term.draw(|f| editor.draw(f, chrome, chat_inactive))?;
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
    queued: VecDeque<String>,
    turn: Option<Turn>,
    /// The `suspended` edge detector for re-asserting raw mode after a modal.
    /// The registration is weak, so the target must be held alongside it.
    arbiter: newt_core::tty::EphemeralRegistration,
    _arbiter_target: Arc<dyn newt_core::tty::Ephemeral>,
    was_suspended: bool,
    /// True only while a blocking interaction owns the keyboard. The editor
    /// remains visible, but its prompt marker recedes behind the modal.
    chat_inactive: bool,
    dirty: bool,
    last_draw: Instant,
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
        execute!(
            stdout,
            crossterm::event::EnableBracketedPaste,
            DisableLineWrap
        )?;
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
        let tty = capture.tty().try_clone()?;
        let backend = CrosstermBackend::new(tty.try_clone()?);
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
            queued: VecDeque::new(),
            turn: None,
            arbiter,
            _arbiter_target: ephemeral,
            was_suspended: false,
            chat_inactive: false,
            dirty: true,
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
                let plain_body = newt_core::markup::plain::render(&interaction.definition);
                let requested_rows = modal_requested_rows(&plain_body, self.screen.cols);
                let reservation = self.screen.reserve_modal_rows(requested_rows)?;
                // Transfer visible focus before the blocking read: the modal's
                // chevron becomes active and the persistent chat chevron
                // recedes without discarding its draft. When a very short
                // terminal cannot fit both surfaces, the chat box is hidden
                // until the modal closes instead of being overwritten.
                self.chat_inactive = true;
                if reservation.chat_visible {
                    if let Err(error) = self.draw() {
                        self.chat_inactive = false;
                        return Err(error);
                    }
                }
                if let Err(error) = self.screen.place_cursor(reservation.start) {
                    self.chat_inactive = false;
                    let _ = self.screen.cleanup_modal(&reservation);
                    let _ = self.draw();
                    return Err(error);
                }
                // fd 1 is the cockpit's captured PTY. Route this blocking
                // interaction to the saved real terminal instead, otherwise
                // its bytes would wait in the capture until this same loop
                // returned from the read — a prompt visible only after it was
                // answered.
                let window = newt_core::tty::Terminal::suspend_for_prompt_to(
                    prompt_output,
                    newt_core::tty::TerminalTaker::CockpitModal,
                );
                let (outcome, prompt_notice) = crate::permissions::present_on_terminal_with_width(
                    &window,
                    &interaction,
                    usize::from(self.screen.cols),
                );
                drop(window);
                let modal_cleanup = self.screen.cleanup_modal(&reservation);
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
            // The panel sibling of `Interact`. Same reservation, same focus
            // transfer, same repaint — the difference is only WHO draws in the
            // reserved rows: the presenter renders a semantic interaction
            // itself, while a panel is lent the rows and draws its own loop on
            // the session thread. This arm PARKS the presenter for that whole
            // time, which is what keeps two writers off one terminal and
            // leaves the keyboard to the panel.
            SurfaceRequest::Panel { rows, reply } => {
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
                let reservation = self.screen.reserve_modal_rows(rows)?;
                self.chat_inactive = true;
                if reservation.chat_visible {
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
                    reservation.start,
                    reservation.rows,
                    self.screen.cols,
                    Some(release),
                );
                if reply.send(Some(window)).is_err() {
                    // The session vanished between asking and receiving. Undo
                    // the reservation rather than parking forever.
                    self.chat_inactive = false;
                    let cleanup = self.screen.cleanup_modal(&reservation);
                    let _ = self.screen.term.clear();
                    let _ = self.draw();
                    return cleanup;
                }
                // Park. A `RecvError` means the window was dropped without a
                // send — the same "the panel is done" signal, reached by a
                // path that could not send. Either way: clean up.
                let _ = released.recv();
                let modal_cleanup = self.screen.cleanup_modal(&reservation);
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
        if ready <= 0 || fds[0].revents & libc::POLLIN == 0 {
            return Ok(());
        }
        // Drain every event crossterm has parsed so far.
        while event::poll(Duration::from_millis(1))? {
            let evt = event::read()?;
            self.on_event(evt)?;
        }
        Ok(())
    }

    fn on_event(&mut self, evt: Event) -> io::Result<()> {
        self.dirty = true;
        match evt {
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
            other => {
                let outcome = self.editor.on_event(other, &mut self.screen)?;
                if let Some(outcome) = outcome {
                    self.on_outcome(outcome);
                }
                Ok(())
            }
        }
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
    fn modal_reservation_sits_immediately_above_the_chat_block() {
        assert_eq!(
            plan_modal_reservation(20, 24, 5),
            ModalReservation {
                start: 15,
                rows: 5,
                chat_visible: true,
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
                start: 0,
                rows: 4,
                chat_visible: false,
            }
        );
    }

    #[test]
    fn modal_reservation_counts_wrapped_display_rows() {
        assert_eq!(modal_physical_rows("short", 10), 1);
        assert_eq!(modal_physical_rows("123456", 5), 2);
        assert_eq!(
            modal_physical_rows("ab界\n\nlast", 3),
            5,
            "wide cells, hard newlines, and blank lines all occupy rows"
        );
        assert_eq!(
            modal_requested_rows("123456", 5),
            3,
            "two wrapped body rows plus the answer row"
        );
        assert_eq!(
            modal_requested_rows("x", 1),
            2,
            "the editable answer stays on one clipped, no-wrap row"
        );
    }

    #[test]
    fn full_screen_modal_cleanup_clears_outside_the_fixed_chat_viewport() {
        let cleanup = full_screen_modal_cleanup().expect("cleanup bytes");
        let mut expected = Vec::new();
        queue!(expected, MoveTo(0, 0), Clear(ClearType::All)).expect("expected bytes");
        assert_eq!(cleanup, expected);
        assert!(
            cleanup.windows(4).any(|window| window == b"\x1b[2J"),
            "cleanup must clear the whole screen, not only ratatui's fixed viewport"
        );
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
mod terminal_acceptance {
    use super::*;
    use crate::cockpit::test_tty::{
        echoes, is_canonical, mode_diff, modes_equal, set_canonical_echo, termios_of, TestTty,
    };

    const CTRL_C: &[u8] = &[0x03];

    /// The three properties #1744 turns on, proven on one real terminal with
    /// one cockpit: Ctrl-C's two tiers, a modal opening underneath it, and the
    /// terminal handed back exactly as it was found.
    ///
    /// #1959: also serialized on `prompt_stdin` — this test constructs a real
    /// `PromptWindow` via `Terminal::suspend_for_prompt`, which bumps the same
    /// process-global counter
    /// `permission_prompt_tests::headless_and_piped_sessions_never_construct_a_prompt_window`
    /// asserts is untouched.
    #[serial_test::serial(tty_arbiter, prompt_stdin)]
    #[test]
    fn the_cockpit_owns_the_terminal_correctly_and_gives_it_back() {
        let tty = TestTty::install();
        newt_core::tty::set_interrupt_pending(false);

        // A shell's terminal: canonical, echoing.
        set_canonical_echo(0);
        let before = termios_of(0);
        assert!(
            is_canonical(0) && echoes(0),
            "precondition: the terminal starts as a shell hands it over"
        );

        let dir =
            std::env::temp_dir().join(format!("newt-cockpit-acceptance-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let surface =
            crate::rich_input::RichSurface::new(Some(dir.join("history"))).expect("rich surface");

        {
            let mut cockpit = match Presenter::open(surface) {
                Ok(p) => p,
                Err(e) => panic!(
                    "cockpit failed to open: {e}; master saw {:?}",
                    tty.painted()
                ),
            };

            // ---- the terminal is genuinely taken ----
            assert!(!is_canonical(0), "the cockpit runs the terminal raw");
            assert!(!echoes(0), "and the kernel is not echoing over the editor");

            // ---- Ctrl-C: every press trips the cancel and is counted ----
            let cancel = Arc::new(AtomicBool::new(false));
            cockpit
                .handle_request(SurfaceRequest::TurnStarted {
                    cancel: Arc::clone(&cancel),
                })
                .expect("a turn starts");

            tty.type_bytes(CTRL_C);
            cockpit.poll_keys().expect("first Ctrl-C");
            assert!(
                cancel.load(Ordering::SeqCst),
                "first press trips the cancel the session races against"
            );
            // Operator-observable: this is the count the spinner reads to swap
            // its stage label, so the press is acknowledged on screen.
            assert_eq!(
                newt_core::tty::interrupt_presses(),
                1,
                "first press raises the acknowledgment the operator sees"
            );

            tty.type_bytes(CTRL_C);
            cockpit.poll_keys().expect("second Ctrl-C");
            // #2010: the second press is HEARD — it bumps the count the
            // spinner renders, so it is visibly different from the first
            // instead of being absorbed into a flag read after the turn.
            assert_eq!(
                newt_core::tty::interrupt_presses(),
                2,
                "the second press is acknowledged at press time"
            );

            cockpit
                .handle_request(SurfaceRequest::TurnEnded)
                .expect("the turn ends");
            assert!(
                !newt_core::tty::interrupt_pending(),
                "ending the turn clears the acknowledgment, so the next turn \
                 does not open already showing it"
            );

            // ---- a modal is visible before an answer while the cockpit owns
            //      the terminal, then focus and the draft return to chat ----
            // Push the mounted block to the bottom, then queue a chrome change
            // that grows it by one row without drawing in between. `run`
            // drains requests in exactly that order; the interaction must
            // synchronize the pending layout before reserving its own rows.
            cockpit
                .screen
                .insert_rows(vec![b"transcript row".to_vec(); 24])
                .expect("push the cockpit block to the bottom");
            cockpit.draw().expect("seat the bottom-anchored block");
            let stale_top = cockpit.screen.top;
            {
                let (editor, screen) = (&mut cockpit.editor, &mut cockpit.screen);
                editor
                    .on_event(Event::Paste("draft survives".into()), screen)
                    .expect("prefill the mounted draft");
            }
            let draft = cockpit.editor.draft();
            cockpit
                .handle_request(SurfaceRequest::SetBackgroundJobs(vec![
                    crate::chat::BackgroundJob::start("indexing repository"),
                ]))
                .expect("queue a chrome row before the modal");
            assert!(cockpit.dirty, "the queued chrome has not drawn yet");
            let expected_status_rows = cockpit.status_rows();
            let expected_editor_rows = cockpit.editor.wanted_rows(
                cockpit.screen.cols,
                cockpit.screen.rows,
                &cockpit.surface.chrome(),
            );
            let expected_block_h =
                (expected_editor_rows + expected_status_rows).clamp(1, cockpit.screen.rows.max(1));
            let expected_top = if stale_top + expected_block_h > cockpit.screen.rows {
                cockpit.screen.rows - expected_block_h
            } else {
                stale_top
            };
            assert!(
                expected_top < stale_top,
                "the regression needs the pending chrome row to move the block: \
                 stale={stale_top}, expected={expected_top}"
            );
            let definition = crate::permissions::free_text_form("Cockpit modal visible?");
            let plain_body = newt_core::markup::plain::render(&definition);
            let requested_rows = modal_requested_rows(&plain_body, cockpit.screen.cols);
            let expected_reservation =
                plan_modal_reservation(expected_top, cockpit.screen.rows, requested_rows);
            assert!(
                expected_reservation.chat_visible,
                "acceptance must exercise reserved rows with the inactive chat still visible: {expected_reservation:?}"
            );
            // Where THIS round's bytes begin. The assertions below ask what
            // the modal wrote, and the buffer also holds what everything
            // before it wrote — including, when the harness orders the two
            // serialized cockpit tests the other way round (which coverage
            // instrumentation does), the previous test's terminal RESTORE.
            // That restore legitimately re-enables line wrap, so a whole-buffer
            // scan for `EnableLineWrap` was reading another test's teardown as
            // this modal's behavior.
            let before_modal_round = tty.painted().len();
            let typer = tty.type_when_painted("Prompt — Cockpit modal visible?", b"yes\r");
            let (reply, answer) = std::sync::mpsc::sync_channel(1);
            cockpit
                .handle_request(SurfaceRequest::Interact {
                    interaction: Box::new(
                        newt_core::interaction_surface::SurfaceInteraction::blocking(definition),
                    ),
                    reply,
                })
                .expect("present the interaction");
            assert!(
                typer.join().expect("prompt watcher"),
                "the modal must reach the real terminal before input is sent; painted: {:?}",
                tty.painted()
            );
            assert_eq!(
                answer.recv().expect("interaction answer"),
                newt_core::HumanQuestionOutcome::Answer("yes".into())
            );
            assert_eq!(cockpit.editor.draft(), draft, "the chat draft survives");
            assert!(!cockpit.chat_inactive, "keyboard focus returns to chat");
            assert_eq!(
                cockpit.screen.top, expected_top,
                "the pending layout is applied before modal reservation"
            );
            let provisional = tty.painted();
            let prompt_at = provisional
                .find("Prompt — Cockpit modal visible?")
                .expect("modal body reached the terminal");
            let mut show = Vec::new();
            queue!(show, crossterm::cursor::Show).expect("show-cursor bytes");
            let show = String::from_utf8(show).expect("show bytes are UTF-8");
            // #1959 (post-rebase flake): the modal's OPENING is already
            // synchronized (`type_when_painted` above waits for its prompt
            // text before this point), but the repaint that restores the
            // chat cursor when it CLOSES is the last thing this round
            // writes, with no synchronization point before the snapshot
            // below — unlike input, `painted()` has no way to know the
            // responder thread (which drains the pty master on its own
            // thread) has caught up with a write that already returned.
            // Proved directly, not assumed: 15 concurrent runs of this exact
            // test, 6 failed here, each truncated at a DIFFERENT byte
            // offset — the signature of a drain race, not a fixed defect.
            // Waiting for the exact evidence the assertion below checks for
            // changes WHEN it is safe to read, not WHAT is asserted.
            assert!(
                tty.wait_for_painted_after(prompt_at, &show, std::time::Duration::from_secs(2)),
                "the chat cursor was not restored after the modal closed (waited 2s): {:?}",
                tty.painted()
            );
            let painted = tty.painted();
            let mut expected_move = Vec::new();
            queue!(expected_move, MoveTo(0, expected_reservation.start))
                .expect("cursor-placement bytes");
            let expected_move = String::from_utf8(expected_move).expect("cursor bytes are UTF-8");
            assert!(
                painted[..prompt_at].contains(&expected_move),
                "the modal was not placed in its reserved rows above chat; expected {expected_move:?}, painted: {painted:?}"
            );
            let mut hide = Vec::new();
            queue!(hide, crossterm::cursor::Hide).expect("hide-cursor bytes");
            let hide = String::from_utf8(hide).expect("hide bytes are UTF-8");
            let before_modal = &painted[..prompt_at];
            assert!(
                before_modal.rfind(&hide) > before_modal.rfind(&show),
                "the mounted chat cursor must recede before the modal takes focus: {painted:?}"
            );
            assert!(
                painted[prompt_at..].contains(&show),
                "the chat cursor was not restored after the modal closed: {painted:?}"
            );
            let mut enable_wrap = Vec::new();
            queue!(enable_wrap, EnableLineWrap).expect("enable-wrap bytes");
            let enable_wrap = String::from_utf8(enable_wrap).expect("wrap bytes are UTF-8");
            assert!(
                !painted[before_modal_round..].contains(&enable_wrap),
                "a modal must keep terminal autowrap disabled so a long answer cannot spill into chat: {:?}",
                &painted[before_modal_round..]
            );

            // A slash command entered in a modal backs out to chat and leaves
            // corrective guidance in durable transcript space. The old path
            // wrote this notice through PromptWindow on the first chat row;
            // the immediate cockpit repaint then erased it.
            let slash_definition =
                crate::permissions::free_text_form("Slash commands stay in chat?");
            let slash_top = cockpit.screen.top;
            let slash_block_h = cockpit.screen.block_h;
            let slash_rows = cockpit.screen.rows;
            let slash_cols = cockpit.screen.cols;
            let before_slash = tty.painted().len();
            let slash_typer =
                tty.type_when_painted("Prompt — Slash commands stay in chat?", b"/help\r");
            let (slash_reply, slash_answer) = std::sync::mpsc::sync_channel(1);
            cockpit
                .handle_request(SurfaceRequest::Interact {
                    interaction: Box::new(
                        newt_core::interaction_surface::SurfaceInteraction::blocking(
                            slash_definition,
                        ),
                    ),
                    reply: slash_reply,
                })
                .expect("present the slash-command interaction");
            assert!(
                slash_typer.join().expect("slash prompt watcher"),
                "the slash-command prompt must be visible before input"
            );
            assert_eq!(
                slash_answer.recv().expect("slash-command outcome"),
                newt_core::HumanQuestionOutcome::Cancelled,
                "a slash command is chat intent, not a modal answer"
            );

            let notice_row = crate::permissions::SLASH_COMMAND_PROMPT_NOTICE
                .as_bytes()
                .to_vec();
            let notice_rows = wrap_row(&notice_row, usize::from(slash_cols));
            let (committed_notice, _) =
                render_insert(slash_top, slash_block_h, slash_rows, &notice_rows)
                    .expect("durable notice insertion plan");
            let committed_notice =
                String::from_utf8(committed_notice).expect("notice bytes are UTF-8");
            // #1959 (post-rebase flake, same shape as the cursor-restore wait
            // above): the notice-commit repaint is the last thing this round
            // writes, with no synchronization point before a snapshot taken
            // right after `handle_request` returns. Wait for the exact
            // evidence the assertion below checks for.
            assert!(
                tty.wait_for_painted_after(
                    before_slash,
                    &committed_notice,
                    std::time::Duration::from_secs(2)
                ),
                "slash guidance was not committed above the cockpit viewport (waited 2s): {:?}",
                &tty.painted()[before_slash..]
            );
            let painted_after_slash = tty.painted();
            let slash_delta = &painted_after_slash[before_slash..];
            assert!(
                slash_delta.contains(&committed_notice),
                "slash guidance was not committed above the cockpit viewport: {slash_delta:?}"
            );
            assert_eq!(
                cockpit.editor.draft(),
                draft,
                "backing out of a modal keeps the mounted chat draft"
            );

            // The modal's raw reader still composes with the cockpit's own
            // raw-mode guard. This checks termios directly, independently of
            // the visibility assertion above.
            // The integration #1770 could not prove alone: that fix made the
            // modal take raw mode from the real termios instead of crossterm's
            // global, and the cockpit is exactly the second raw-mode owner
            // that broke it. Here the cockpit genuinely holds fd 0/1.
            let window = newt_core::tty::Terminal::suspend_for_prompt(
                newt_core::tty::TerminalTaker::RichSurfaceModal,
            );
            {
                let _reader = newt_core::tty::modal_prompt_controls(&window)
                    .expect("the modal takes the terminal from under the cockpit");
                assert!(
                    !is_canonical(0),
                    "the modal's read must be non-canonical, or a keypress \
                     waits for Enter the operator does not know to press"
                );
                assert!(
                    !echoes(0),
                    "the kernel must not echo the answer over the prompt"
                );
            }
            drop(window);
            // The modal restored what it found: the cockpit still has a raw
            // terminal to go on painting into.
            assert!(
                !is_canonical(0),
                "the cockpit's raw mode survives the modal"
            );

            // ---- and a PANEL is lent rows on the REAL terminal ----
            //
            // The `/backends` defect, at the level only a real terminal can
            // show it: a panel that draws to fd 1 under a mounted cockpit
            // paints into the pty CAPTURE, not onto the screen, and comes back
            // as flattened transcript rows. Nothing mocked can observe that,
            // because the mock IS the capture. Here the panel draws through the
            // window the presenter lends it, and the bytes must appear on the
            // terminal the operator is looking at.
            //
            // The painter runs on its own thread because `handle_request` PARKS
            // until the window drops — that parking is the mechanism keeping
            // two writers off one terminal, so the test has to exercise it
            // rather than sidestep it.
            let (panel_reply, panel_window) =
                std::sync::mpsc::sync_channel::<Option<crate::session_worker::PanelWindow>>(1);
            let painter = std::thread::spawn(move || {
                let window = panel_window
                    .recv()
                    .expect("the presenter answers the panel request")
                    .expect("a mounted cockpit has rows to lend");
                let mut term = window.terminal().expect("a terminal over the lent rows");
                term.draw(|f| {
                    f.render_widget(
                        ratatui::widgets::Paragraph::new("PANEL BODY ON THE REAL TERMINAL"),
                        f.area(),
                    );
                })
                .expect("the panel paints");
                drop(term);
                // Dropping the window is the release; the presenter is parked
                // on it.
                drop(window);
            });
            let panel_plan = plan_modal_reservation(cockpit.screen.top, cockpit.screen.rows, 6);
            assert!(
                panel_plan.chat_visible,
                "acceptance must exercise a panel with the inactive chat still \
                 visible: {panel_plan:?}"
            );
            let before_panel = tty.painted().len();
            cockpit
                .handle_request(SurfaceRequest::Panel {
                    rows: 6,
                    reply: panel_reply,
                })
                .expect("the presenter lends the panel its rows");
            painter.join().expect("panel painter");
            // #1959 (same shape as the cursor-restore and notice-commit waits
            // above): `join` proves the painter's `write` returned, not that
            // the responder thread — the pty master's sole reader — has drained
            // those bytes into `painted`. This is what failed on #2069, a PR
            // that touched a command parser and the slash registry and has no
            // path to cockpit terminal ownership: the delta held the
            // reservation scroll and the chat repaint that precedes the panel,
            // and simply stopped before the panel body.
            //
            // `TERMINAL` is the LAST word the panel paints — ratatui emits
            // cells in row-major order — so once it has landed, every earlier
            // byte of this round is present by stream ordering: the other three
            // words and the presenter's reservation `MoveTo` alike. Waiting on
            // the last evidence changes WHEN the snapshot is safe to read, not
            // WHAT any assertion below demands of it.
            assert!(
                tty.wait_for_painted_after(
                    before_panel,
                    "TERMINAL",
                    std::time::Duration::from_secs(2)
                ),
                "the panel's bytes never reached the real terminal (waited 2s): {:?}",
                &tty.painted()[before_panel..]
            );
            let panel_delta = tty.painted()[before_panel..].to_string();
            // Ratatui emits a cursor move per painted run, so the body arrives
            // as words rather than one string. What matters is that they
            // arrive AT ALL (they would be swallowed by the capture) and that
            // they land in the rows the presenter reserved.
            let mut panel_move = Vec::new();
            queue!(panel_move, MoveTo(0, panel_plan.start)).expect("panel cursor bytes");
            let panel_move = String::from_utf8(panel_move).expect("panel cursor bytes are UTF-8");
            assert!(
                panel_delta.contains(&panel_move),
                "the panel must be placed in its reserved rows above chat; \
                 expected {panel_move:?}, painted: {panel_delta:?}"
            );
            for word in ["PANEL", "BODY", "REAL", "TERMINAL"] {
                assert!(
                    panel_delta.contains(word),
                    "the panel's bytes must reach the real terminal, not the \
                     cockpit's fd 1 capture; missing {word:?}: {panel_delta:?}"
                );
            }
            assert!(
                !cockpit.chat_inactive,
                "keyboard focus returns to chat when the panel window drops"
            );
            assert_eq!(
                cockpit.editor.draft(),
                draft,
                "the chat draft survives a panel"
            );
            assert!(
                !is_canonical(0),
                "the cockpit's raw mode survives the panel"
            );

            // ---- the rows come back on the paths nobody plans for ----
            //
            // Release-on-drop is the whole synchronization mechanism, so the
            // paths worth proving are the ones with no drawing in them at all:
            // a panel that fails before its first frame, and a session that
            // vanishes between asking for rows and taking them. Either would
            // strand the presenter parked on rows nobody will release, which
            // presents as a frozen cockpit rather than as an error.
            let (early_reply, early_window) =
                std::sync::mpsc::sync_channel::<Option<crate::session_worker::PanelWindow>>(1);
            let early = std::thread::spawn(move || {
                // Took the rows, drew nothing, gave them straight back — the
                // shape of a panel whose seed was empty or whose terminal
                // failed to build.
                drop(early_window.recv().expect("the presenter answers"));
            });
            cockpit
                .handle_request(SurfaceRequest::Panel {
                    rows: 6,
                    reply: early_reply,
                })
                .expect("an undrawn panel still returns");
            early.join().expect("early-drop panel");
            assert!(
                !cockpit.chat_inactive,
                "focus returns when a panel drops its window without drawing"
            );

            // And the session that disappears mid-ask: the reply send fails,
            // which must undo the reservation rather than park on it.
            let (orphan_reply, orphan_window) =
                std::sync::mpsc::sync_channel::<Option<crate::session_worker::PanelWindow>>(1);
            drop(orphan_window);
            cockpit
                .handle_request(SurfaceRequest::Panel {
                    rows: 6,
                    reply: orphan_reply,
                })
                .expect("a vanished session is not a presenter error");
            assert!(
                !cockpit.chat_inactive,
                "a panel request nobody received must not leave chat dimmed"
            );
            assert_eq!(
                cockpit.editor.draft(),
                draft,
                "the chat draft survives both panel failure paths"
            );
            // **The block is still LIVE after both failure paths** — it takes
            // input and paints it, rather than being stuck in the geometry the
            // failed panel reserved.
            //
            // Proven by typing something new, not by `dirty = true` + a length
            // comparison. Ratatui diffs its buffer: a redraw of unchanged
            // content emits a handful of trailing bytes and nothing else, so
            // the length check was asserting almost nothing — and what little
            // it did assert raced the responder thread's drain, which is how
            // it failed under coverage instrumentation. New text can only
            // reach the terminal through a live block, and
            // `wait_for_painted_after` reads at a point where the drain has
            // caught up.
            let before_repaint = tty.painted().len();
            {
                let (editor, screen) = (&mut cockpit.editor, &mut cockpit.screen);
                editor
                    .on_event(Event::Paste(" and still alive".into()), screen)
                    .expect("the mounted editor still takes input");
            }
            cockpit
                .draw()
                .expect("the cockpit still paints after a failed panel");
            assert!(
                tty.wait_for_painted_after(
                    before_repaint,
                    "alive",
                    std::time::Duration::from_secs(2)
                ),
                "the cockpit block is still live after the panel failure paths \
                 (waited 2s): {:?}",
                &tty.painted()[before_repaint..]
            );
        }

        // ---- and the terminal comes back, exactly ----
        assert!(is_canonical(0), "canonical mode restored on teardown");
        assert!(echoes(0), "echo restored on teardown");
        assert!(
            modes_equal(&before, &termios_of(0)),
            "every termios mode field restored exactly, not approximately — \
             this is the assertion an emitted-escape-bytes check cannot make; \
             differs: {}",
            mode_diff(&before, &termios_of(0))
        );
    }

    /// Acceptance (#1744): the same restoration through an ABNORMAL exit.
    ///
    /// Proven against the modes guard itself rather than a second cockpit: the
    /// guard is the mechanism (`Presenter::open` binds it before the fallible
    /// capture install precisely so a `?` or a panic cannot strand the
    /// terminal), and one cockpit per process is a harness limit, not a reason
    /// to leave the unwind path unproven.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn a_panic_restores_the_real_termios_through_the_modes_guard() {
        let _tty = TestTty::install();
        // Clear crossterm's saved-mode static FIRST. It is process-global, so
        // an earlier test in this binary may have populated it.
        //
        // #1925 is what makes this belt and braces rather than load-bearing:
        // the presenter takes raw through `RawModeGuard`, which captures the
        // real termios and never consults that static. The hazard this line
        // guards against — restoring an OLDER baseline than the one this test
        // set — is the very defect the swap removes. Kept because other tests
        // in this binary still populate the static.
        let _ = crossterm::terminal::disable_raw_mode();
        set_canonical_echo(0);
        let before = termios_of(0);

        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(|| {
            // Model the PRESENTER'S OWN FIELD PAIR, in its declaration order
            // (#1925). A struct, not two locals, and deliberately: struct
            // fields drop in declaration order while locals drop in REVERSE,
            // so two `let`s here would exercise the opposite order to the one
            // the presenter has and prove nothing about it.
            struct Held {
                _restore: crate::RestoreOnDrop<fn()>,
                _raw: newt_core::tty::raw_mode::RawModeGuard,
            }
            let _held = Held {
                _restore: crate::RestoreOnDrop {
                    restore: restore_terminal_modes,
                },
                _raw: newt_core::tty::raw_mode::RawModeGuard::enter().expect("raw"),
            };
            assert!(!is_canonical(0), "precondition: raw mode taken");
            panic!("turn exploded while the terminal was raw");
        });
        std::panic::set_hook(hook);

        assert!(
            result.is_err(),
            "the panic must propagate, not be swallowed"
        );
        assert!(
            is_canonical(0),
            "canonical mode restored through the unwind"
        );
        assert!(echoes(0), "echo restored through the unwind");
        assert!(
            modes_equal(&before, &termios_of(0)),
            "every termios mode field restored exactly after a panic; differs: {}",
            mode_diff(&before, &termios_of(0))
        );
    }
}
