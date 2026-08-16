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
//! `suspended` false edge. Ctrl-C during a turn interrupts (Esc belongs to
//! vi); the second press escalates, matching the watcher.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color as CColor, ResetColor, SetAttribute, SetForegroundColor};
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

    /// Insert finished rows into the transcript above the block.
    ///
    /// Erase the block, write the rows from `top` (each pre-wrapped, each
    /// followed by a style reset and `\r\n`), scroll whatever is needed so
    /// the last row ends just above where the block will sit, then move the
    /// block. All arithmetic, no queries — see the module docs.
    fn insert_rows(&mut self, rows: Vec<Row>) -> io::Result<()> {
        let cols = self.cols as usize;
        let phys: Vec<Row> = rows.iter().flat_map(|r| wrap_row(r, cols)).collect();
        let Ok(k) = u16::try_from(phys.len()) else {
            return Ok(());
        };
        if k == 0 {
            return Ok(());
        }
        let plan = plan_insert(self.top, self.block_h, self.rows, k);
        let mut buf = Vec::with_capacity(phys.iter().map(Vec::len).sum::<usize>() + 64);
        queue!(buf, MoveTo(0, self.top), Clear(ClearType::FromCursorDown))?;
        for row in &phys {
            buf.extend_from_slice(row);
            // A reset per row: styling from the transcript must never leak
            // into the next row or into the block.
            buf.extend_from_slice(b"\x1b[0m\r\n");
        }
        if plan.extra_scroll > 0 {
            queue!(buf, MoveTo(0, self.rows.saturating_sub(1)))?;
            buf.extend(std::iter::repeat_n(b'\n', plan.extra_scroll as usize));
        }
        self.tty.write_all(&buf)?;
        self.tty.flush()?;
        let moved = plan.new_top != self.top;
        self.top = plan.new_top;
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
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        let new_h = (editor_rows + status_rows).clamp(1, self.rows);
        self.block_h = new_h;
        self.status_rows = status_rows;
        self.top = self.rows - new_h;
        let mut buf = Vec::new();
        queue!(buf, MoveTo(0, self.top), Clear(ClearType::FromCursorDown))?;
        self.tty.write_all(&buf)?;
        self.tty.flush()?;
        self.rebuild_term()
    }

    /// Paint the block: the status row (raw, clipped, with the queued chip at
    /// the right edge), then the ratatui viewport.
    fn draw(&mut self, editor: &MountedEditor, chrome: Chrome<'_>) -> io::Result<()> {
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
        self.term.draw(|f| editor.draw(f, chrome))?;
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
        let _ = crossterm::terminal::disable_raw_mode();
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

/// Pure geometry — the part of `insert_rows` that must be exactly right and
/// can be pinned without a terminal.
///
/// Rows are written from `top`; each `\r\n` at the last screen row scrolls
/// by one. Afterwards the last written row sits at `min(top+k-1, rows-1)`;
/// the block wants to start right after it, but no lower than
/// `rows - block_h`, so whatever overshoot there is becomes extra scroll.
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

/// A turn in flight: the flags the session races its work against.
struct Turn {
    cancel: Arc<AtomicBool>,
    hard: Arc<AtomicBool>,
    presses: u32,
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
    dirty: bool,
    last_draw: Instant,
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
        let (x, y) = crossterm::cursor::position()?;
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
        crossterm::terminal::enable_raw_mode()?;
        execute!(
            stdout,
            crossterm::event::EnableBracketedPaste,
            DisableLineWrap
        )?;
        let capture = PtyCapture::install(cols, rows)?;
        let tty = capture.tty().try_clone()?;
        let backend = CrosstermBackend::new(tty.try_clone()?);
        let term = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, top, cols, block_h)),
            },
        )?;
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
            dirty: true,
            last_draw: Instant::now(),
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

    fn handle_request(&mut self, req: SurfaceRequest) -> io::Result<()> {
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
                self.editor = MountedEditor::new(
                    self.surface.edit(),
                    self.surface.gutter(),
                    self.surface.history(),
                    &draft,
                );
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
            SurfaceRequest::TurnStarted { cancel, hard } => {
                self.turn = Some(Turn {
                    cancel,
                    hard,
                    presses: 0,
                });
                self.dirty = true;
            }
            SurfaceRequest::TurnEnded => {
                self.turn = None;
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
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('c')
                    && self.turn.is_some() =>
            {
                // Ctrl-C during a turn interrupts — the draft is kept. The
                // first press asks; the second forces. Same tiers as the
                // watcher, and the spinner label acknowledges within a tick.
                if let Some(turn) = self.turn.as_mut() {
                    turn.presses += 1;
                    if turn.presses == 1 {
                        turn.cancel.store(true, Ordering::SeqCst);
                        newt_core::tty::set_interrupt_pending(true);
                    } else {
                        turn.hard.store(true, Ordering::SeqCst);
                    }
                }
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
            if !suspended {
                // Belt and braces: the modal restores the exact prior termios
                // itself now, but a stray `disable_raw_mode` anywhere would
                // otherwise leave us cooked. A no-op when already raw.
                let _ = crossterm::terminal::enable_raw_mode();
            }
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
        self.screen.draw(&self.editor, self.surface.chrome())?;
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
    fn a_styled_line_round_trips_to_ansi_with_a_reset_per_span() {
        use ratatui::style::{Color, Style};
        use ratatui::text::Span;
        let line = Line::from(vec![
            Span::styled("[t]", Style::default().fg(Color::DarkGray)),
            Span::raw(" body"),
        ]);
        let bytes = line_to_ansi(&line).unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("[t]"), "{s:?}");
        assert!(s.contains(" body"), "{s:?}");
        assert!(s.contains("\x1b[0m"), "reset present: {s:?}");
        assert_eq!(super::super::ansi::visible_width(&bytes), "[t] body".len());
    }
}
