//! **One answer to "where is the cursor?" for every inline surface** (#1950).
//!
//! An inline viewport has to know which row it starts on, so ratatui asks the
//! terminal — `backend.get_cursor_position()`, which is a DSR query (`ESC[6n`)
//! whose reply arrives back on the *input* stream. Any other consumer of that
//! stream can take the reply first, and some terminals never send one. When
//! that happens crossterm returns
//! `"The cursor position could not be read within a normal duration"` after a
//! two-second timeout and `Terminal::with_options` fails — so the panel does
//! not open at all.
//!
//! That is what the operator hit on `/backends` (#1950).
//!
//! # The mechanism is NOT raw mode
//!
//! #1950 was filed against `RawModeGuard` (#1924) on the theory that setting
//! raw via `libc::tcsetattr` leaves crossterm's own raw-mode flag unset, so
//! `cursor::position()` takes its not-raw branch and mis-reads. The premise is
//! true — the flag really is unset — but it is **not** the cause, and it was
//! measured rather than argued:
//!
//! | raw mode taken by | competing reader | outcome |
//! |---|---|---|
//! | `RawModeGuard` (flag `false`) | none | opens, 14 ms |
//! | `crossterm::enable_raw_mode` (flag `true`) | none | opens, 17 ms |
//! | `RawModeGuard` (flag `false`) | one | **fails, 2000 ms** |
//! | `crossterm::enable_raw_mode` (flag `true`) | one | **fails, 2000 ms** |
//!
//! The pre-#1924 shape fails identically, so syncing the flag would fix
//! nothing. Reading crossterm 0.28 says why: *both* branches of `position()`
//! end in the same `read_position_raw()`; the not-raw branch only wraps it in
//! an `enable_raw_mode`/`disable_raw_mode` pair that is a no-op when the
//! terminal is already raw. Two other candidate differences were checked and
//! eliminated too — crossterm's `raw_terminal_attr` *is* `cfmakeraw`, and its
//! `tty_fd()` returns `STDIN_FILENO` whenever stdin is a tty, which is the
//! same descriptor `RawModeGuard` uses.
//!
//! # So this fixes the class, not the instance
//!
//! Whichever consumer wins the race — newt's own interrupt watcher, a
//! multiplexer that owns the outer pty, a terminal that simply does not answer
//! — the surface should still open. **Degrade, do not refuse.** A panel drawn
//! at a slightly wrong row beats a panel that will not appear.
//!
//! # Why here and not at four call sites
//!
//! `config_panel`, `rich_input`, and `interaction_view` each built the same
//! `Terminal::with_options(CrosstermBackend::new(stdout), Inline(h))`, and
//! `cockpit::presenter` calls `cursor::position()` directly. A rescue written
//! into each is four copies of one rule — the shape F0b (#1923) spent a slice
//! removing. The seam ratatui actually calls is the **backend**, so the rule
//! lives in one `Backend` implementation and every inline surface inherits it.

use std::io::{self, Stdout, Write};

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::{Terminal, TerminalOptions, Viewport};

/// The terminal type every inline surface in this crate uses.
pub(crate) type InlineTerm = Terminal<AnchoredBackend<Stdout>>;

/// Say it once per process, before anything paints.
///
/// Same idiom, and for the same reason, as the cbreak warning in
/// `with_live_spill_watch`: losing a terminal capability silently is how a
/// wrong-looking surface becomes undiagnosable. Once, because the operator's
/// report already showed a config warning printing three times per command,
/// and a fourth repeated line is noise rather than information.
fn warn_once(err: &io::Error) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "⚠ terminal did not report the cursor position ({err}) — inline \
             panels will be anchored at the bottom of the screen"
        );
    });
}

/// Where to draw when the terminal will not say where the cursor is.
///
/// **The assumption, stated because a silent one rots:** that the cursor is at
/// **column 0 of the last row** — where a shell prompt sits after output, and
/// where every one of these surfaces is opened from. If the cursor was really
/// higher up, ratatui's `compute_inline_size` appends its lines from here and
/// the panel is drawn at the bottom of the screen instead of inline at the
/// cursor: the content is right, the position is lower than ideal. If the
/// screen height is unknown too, `(0, 0)` is the only remaining answer and the
/// panel draws at the top.
fn anchor(size: Size) -> Position {
    Position {
        x: 0,
        y: size.height.saturating_sub(1),
    }
}

/// A `CrosstermBackend` that answers "where is the cursor?" even when the
/// terminal does not.
///
/// Every other method delegates untouched — this exists to change exactly one
/// answer, and a wrapper that quietly changed a second would be worse than the
/// four copies it replaces.
pub(crate) struct AnchoredBackend<W: Write> {
    inner: CrosstermBackend<W>,
}

impl<W: Write> AnchoredBackend<W> {
    pub(crate) fn new(writer: W) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
        }
    }
}

impl<W: Write> Backend for AnchoredBackend<W> {
    /// **The one overridden answer.** A failure here is a terminal that did
    /// not reply, not a broken terminal: fall back and keep going.
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        match self.inner.get_cursor_position() {
            Ok(position) => Ok(position),
            Err(err) => {
                warn_once(&err);
                // `size()` is an ioctl, not a query — it does not depend on
                // the terminal answering anything, so it is still trustworthy
                // in exactly the situation that got us here.
                Ok(anchor(self.inner.size().unwrap_or(Size {
                    width: 0,
                    height: 0,
                })))
            }
        }
    }

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
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
        // Disambiguated: `CrosstermBackend` implements both `Backend::flush`
        // and `Write::flush`, and this is the Backend one.
        Backend::flush(&mut self.inner)
    }
}

// `scroll_region_up`/`scroll_region_down` are ratatui's `scrolling-regions`
// feature, which this build does not enable; the trait's provided
// implementations stand. Adding them here would not compile.

impl<W: Write> Write for AnchoredBackend<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

/// **The ONE inline-viewport constructor.** Every surface in this crate that
/// wants `Viewport::Inline` comes through here, so the rescue above cannot be
/// forgotten by the next one.
pub(crate) fn inline_terminal(height: u16) -> io::Result<InlineTerm> {
    Terminal::with_options(
        AnchoredBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
}

/// The cursor position for a caller that is not building a ratatui terminal —
/// `cockpit::presenter`, which queries it directly to place its block.
///
/// Same rule, same anchor, same one-shot warning: a cockpit that refuses to
/// open because the terminal stayed quiet is the same defect as a panel that
/// does, and it must not be a second implementation of the answer.
pub(crate) fn cursor_position_or_anchor() -> Position {
    let mut backend = AnchoredBackend::new(io::stdout());
    // `get_cursor_position` above already rescues; this is the last resort if
    // even the rescue's own `size()` path errors, and it is the only remaining
    // answer.
    backend
        .get_cursor_position()
        .unwrap_or(Position { x: 0, y: 0 })
}
