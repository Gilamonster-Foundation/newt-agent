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
    /// The rows this backend is allowed to paint (#1979).
    ///
    /// Held here so the terminal OWNS its lease: the rows are returned when
    /// the surface is dropped, in the right order, with no caller to forget.
    /// `None` is the query-only path — [`cursor_position_or_anchor`] asks
    /// where the cursor is and paints nothing, so it holds no rows.
    lease: Option<newt_core::tty::RegionLease>,
}

impl<W: Write> AnchoredBackend<W> {
    /// Query-only: answers "where is the cursor?" and never paints a viewport.
    pub(crate) fn new(writer: W) -> Self {
        Self::with_lease(writer, None)
    }

    fn with_lease(writer: W, lease: Option<newt_core::tty::RegionLease>) -> Self {
        Self {
            lease,
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
                // #1979/#1977: if this surface holds rows, THOSE rows are the
                // anchor. The bare bottom-of-screen answer is what put two
                // viewports on the same rows — the panel and the prompt each
                // asked "where do I go?" and got the same reply.
                //
                // When nothing collided the lease IS the bottom rows, so this
                // returns exactly what `anchor` did and today's panels keep
                // their position. It differs only when the mint shifted the
                // request, which is the case that was broken.
                if let Some(newt_core::tty::Region::Rows { top, .. }) =
                    self.lease.as_ref().map(newt_core::tty::RegionLease::region)
                {
                    return Ok(Position { x: 0, y: top });
                }
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

/// **The ONE inline-viewport constructor, and it takes a lease** (#1979).
///
/// Every surface in this crate that wants `Viewport::Inline` comes through
/// here — F0b (#1923) collapsed four copies into this one — so requiring the
/// capability HERE requires it of all of them in a single edit. There is no
/// bare form to call: the height is read off the lease, because a height that
/// disagreed with the leased rows would be a viewport painting outside what it
/// owns, which is the whole defect (#1977).
///
/// The terminal owns the lease, so the rows are returned on drop with no
/// caller left to forget.
pub(crate) fn inline_terminal(lease: newt_core::tty::RegionLease) -> io::Result<InlineTerm> {
    let height = match lease.region() {
        newt_core::tty::Region::Rows { height, .. } => height,
        // A whole-screen holder is the alternate screen, not an inline strip.
        newt_core::tty::Region::WholeScreen => {
            return Err(io::Error::other(
                "an inline viewport cannot be built from a whole-screen lease",
            ))
        }
    };
    Terminal::with_options(
        AnchoredBackend::with_lease(io::stdout(), Some(lease)),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
}

/// Lease the bottom `height` rows, which is where every inline surface here
/// sits, and say what should happen if somebody already holds them.
///
/// The screen measurement stays with the caller-facing helper rather than in
/// the arbiter: #1979's non-goal is a layout engine, and the arbiter's
/// vocabulary is a row range plus a policy.
pub(crate) fn lease_bottom_rows(
    height: u16,
    policy: newt_core::tty::OnCollision,
) -> io::Result<newt_core::tty::RegionLease> {
    let screen =
        ratatui::backend::Backend::size(&CrosstermBackend::new(io::stdout())).unwrap_or(Size {
            width: 80,
            height: 24,
        });
    let top = screen.height.saturating_sub(height);
    newt_core::tty::Terminal::lease_region(newt_core::tty::Region::Rows { top, height }, policy)
        .ok_or_else(|| io::Error::other("another surface already owns these rows"))
}

/// The cursor position for a caller that is not building a ratatui terminal —
/// `cockpit::presenter`, which queries it directly to place its block.
///
/// Same rule, same anchor, same one-shot warning: a cockpit that refuses to
/// open because the terminal stayed quiet is the same defect as a panel that
/// does, and it must not be a second implementation of the answer.
///
/// **`#[cfg(unix)]` to match its only caller**, not to silence a lint. The
/// live cockpit — the fd 1/2 capture and this presenter — is unix-only by
/// construction (`openpty`/`dup2`/termios); Windows keeps the classic
/// per-turn surface until a ConPTY backend lands (#1746). A
/// `#[allow(dead_code)]` here would say "trust me"; the cfg says which
/// caller, and goes stale in the same commit that gives Windows a cockpit.
///
/// **Windows panels are NOT left unrescued by this.** They reach the same
/// fallback through [`inline_terminal`], which is not gated: `config_panel`,
/// `rich_input` and `interaction_view` are `rich-tui`-gated only, so
/// `AnchoredBackend::get_cursor_position` runs there exactly as it does here.
/// What is absent on Windows is the *cockpit*, not the rescue.
#[cfg(unix)]
pub(crate) fn cursor_position_or_anchor() -> Position {
    let mut backend = AnchoredBackend::new(io::stdout());
    // `get_cursor_position` above already rescues; this is the last resort if
    // even the rescue's own `size()` path errors, and it is the only remaining
    // answer.
    backend
        .get_cursor_position()
        .unwrap_or(Position { x: 0, y: 0 })
}

#[cfg(test)]
mod region_lease_door {
    /// Every file that may construct an inline viewport. **Declared, not
    /// discovered** (F0b): a scan that trusted a glob would silently start
    /// permitting a new file the day someone added one.
    const DESTINATIONS: &[&str] = &["inline_viewport.rs"];

    /// The CALL FORM, never the bare name (#1924). Seven files mention
    /// `Viewport::Inline` in prose — including the doc comment that explains
    /// this very rule — and a name-shaped needle would count all of them, so
    /// the guard would pass by matching its own explanation.
    const DOOR: &str = "Viewport::Inline(";
    const OPTIONS: &str = "Terminal::with_options(";

    fn sources() -> Vec<(String, String)> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = Vec::new();
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).expect("newt-tui/src is readable") {
                let path = entry.expect("a readable entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let name = path
                        .file_name()
                        .expect("a file name")
                        .to_string_lossy()
                        .into_owned();
                    out.push((name, std::fs::read_to_string(&path).expect("readable")));
                }
            }
        }
        out
    }

    /// Region claims that do NOT yet go through a lease.
    ///
    /// A ratchet, not a permission list: the count may only go DOWN. The
    /// cockpit presenter builds two `Viewport::Fixed` regions from its own row
    /// arithmetic (`self.top`, clamped on resize), which is a claim on rows by
    /// any reading — it is simply not this slice's. #1979's sweep takes it,
    /// and `RegionLease::relocate` exists because that block MOVES.
    const UNLEASED_REGION_CLAIMS: &[(&str, usize)] = &[("presenter.rs", 2)];

    /// **The door is one door, and it takes a lease.**
    ///
    /// `inline_terminal` is the only inline-viewport constructor, and its
    /// signature requires a `RegionLease` — so there is no bare form to call
    /// and no second place to add one. This is the `PromptWindow` seal pattern
    /// at region scale: the capability is the way in.
    #[test]
    fn the_inline_viewport_door_exists_in_exactly_one_declared_place() {
        let files = sources();
        // POSITIVE READ ASSERTION FIRST. An absence-check that silently read
        // nothing passes forever, and gets MORE likely to pass as the scan
        // breaks.
        assert!(
            files.len() > 10,
            "the scan found {} files, so it is not reading the crate",
            files.len()
        );
        assert!(
            files.iter().any(|(n, _)| n == "inline_viewport.rs"),
            "the scan never reached the file that holds the door"
        );

        for (name, body) in &files {
            if body.contains(DOOR) {
                assert!(
                    DESTINATIONS.contains(&name.as_str()),
                    "`{DOOR}` appears in {name}, which is not a declared \
                     destination. An inline viewport is a claim on rows: mint a \
                     `RegionLease` and go through `inline_terminal`."
                );
            }
        }
    }

    /// The wider category, ratcheted: every OTHER way a surface claims rows.
    ///
    /// Separate from the door above because the door's baseline is zero from
    /// birth, while this one starts at two and is meant to reach zero in the
    /// sweep. Counting them keeps the mess visible and monotonically
    /// decreasing rather than rewritten.
    #[test]
    fn unleased_region_claims_only_decrease() {
        for (name, body) in &sources() {
            if name == "inline_viewport.rs" {
                continue; // the door itself, and this test's own needles
            }
            let found = body.matches(OPTIONS).count();
            let allowed = UNLEASED_REGION_CLAIMS
                .iter()
                .find(|(f, _)| *f == name)
                .map_or(0, |(_, n)| *n);
            assert!(
                found <= allowed,
                "{name} makes {found} region claims, {allowed} declared. A new \
                 one must mint a `RegionLease`; a removed one must lower the \
                 baseline."
            );
        }
    }

    /// Probe one: the needle WOULD fire on a new site. Without this the test
    /// above passes equally well when the needle matches nothing at all.
    #[test]
    fn the_door_needle_catches_a_bare_construction() {
        let smuggled = "let t = Terminal::with_options(b, TerminalOptions { \
                        viewport: Viewport::Inline(6) });";
        assert!(smuggled.contains(DOOR), "the needle misses a real call");
        assert!(
            smuggled.contains(OPTIONS),
            "the options needle misses a real call"
        );
    }

    /// Probe two: it does NOT fire on prose. Six files discuss
    /// `Viewport::Inline` without constructing one, and a name-shaped needle
    /// would have flagged every one of them — which is how a guard gets
    /// weakened until it means nothing.
    #[test]
    fn the_door_needle_ignores_prose_about_the_rule() {
        let prose = "//! opens this transient ratatui `Viewport::Inline` overlay: one surface";
        assert!(
            !prose.contains(DOOR),
            "the needle matches prose, so the guard would flag documentation"
        );
    }
}
