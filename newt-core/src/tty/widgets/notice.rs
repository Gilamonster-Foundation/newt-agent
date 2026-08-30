//! [`Notice`] — the ONE notice widget. **A value, not a printer.**
//!
//! # What it replaces
//!
//! Seventeen sites across `newt-core` and `newt-tui` each open-code the same
//! twelve-line skeleton:
//!
//! ```ignore
//! if color {
//!     execute!(io::stdout(), SetForegroundColor(hue), Print(msg), ResetColor).ok();
//! } else {
//!     println!("{msg}");
//! }
//! io::stdout().flush().ok();
//! ```
//!
//! Amber alone exists in **four** encodings across them (`DarkYellow`,
//! `Rgb{200,140,0}`, a raw `\x1b[33m`), and `DarkGrey` + `ResetColor` is
//! open-coded roughly twenty-three times. [`Level`] is the one hue table those
//! collapse into.
//!
//! # Why the split into `line()` and `emit()` is the load-bearing part
//!
//! [`Notice::line`] is pure: no ANSI, no I/O, no lock, and therefore testable
//! off the `tty_arbiter` serial lane. [`Notice::emit`] is the only part that
//! touches a terminal, and it routes through [`Terminal::emit_line`] — which
//! cooperates with whoever owns the ephemeral row instead of fighting them.
//!
//! That routing is what makes `newt-tui`'s `summarizer_progress` race
//! *unrepresentable* rather than merely fixed: there is no longer a reason for
//! a notice-producing call site to reach for `io::stdout()` and a hand-rolled
//! `\r ESC[K`, because the shared path is both shorter and correct.
//!
//! # Accessibility
//!
//! `glyph` is not decoration. `display.rs` already records the rule and this
//! type carries it forward: **the glyph carries the meaning; color is never
//! alone.** A notice must remain unambiguous with `color: false`, on a
//! monochrome terminal, and to an operator who cannot resolve the hue — which
//! is why `emit` degrades to the same text rather than to nothing.

use std::borrow::Cow;
use std::io::{self, Write as _};

use crossterm::queue;
use crossterm::style::{Color as CtColor, Print, ResetColor, SetForegroundColor};

use crate::tty::caps::{protocol_mode, LineCaps};
use crate::tty::{LineWriter, Sink, Terminal};

/// The ONE hue table. Six registers, each with a distinct meaning; every
/// notice in the workspace lands in one of them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Level {
    /// Ordinary narration. Default foreground — a colored sigil on every line
    /// reads as noise.
    Info,
    /// Something completed successfully.
    Ok,
    /// The harness speaking about degradation: a retry, a fallback, a trim.
    /// **The one amber**, collapsing `DarkYellow` / `Rgb{200,140,0}` /
    /// `\x1b[33m` into a single encoding.
    Warn,
    /// The last-resort register — silent context loss and its kin. Loud on
    /// purpose; must not be spent on anything recoverable.
    Loud,
    /// Secondary detail that should recede (the reasoning trickle's register).
    Dim,
    /// Diagnostics behind a debug/trace flag.
    Debug,
}

impl Level {
    /// This level's foreground, or `None` for "leave the default alone".
    fn hue(self) -> Option<CtColor> {
        match self {
            Self::Info => None,
            Self::Ok => Some(CtColor::DarkGreen),
            Self::Warn => Some(CtColor::DarkYellow),
            Self::Loud => Some(CtColor::Red),
            Self::Dim | Self::Debug => Some(CtColor::DarkGrey),
        }
    }
}

/// A notice: a level, a meaning-carrying sigil, and its text.
///
/// Construct it, render it with [`Notice::line`], emit it with
/// [`Notice::emit`]. There is deliberately no `redraw` — see the module docs on
/// `tty::widgets`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice<'a> {
    pub level: Level,
    /// The sigil that carries the meaning (`⚠`, `✓`, `⧉`, `⛔`, `↻`). May be
    /// empty for a notice whose text already leads with its own marker.
    pub glyph: &'a str,
    /// Spaces between glyph and text. Both of the workspace's conventions are
    /// in use and both must render byte-identically: the `newt-tui` summarizer
    /// notices use one (`"⚠ summarizer failed…"`), while the `agentic::display`
    /// family uses two (`"✓  context summarized…"`, `"⚠  newt: …"`) to give the
    /// East-Asian-Ambiguous sigils — which some terminals render two cells wide
    /// — a stable text column.
    pub gap: usize,
    pub text: Cow<'a, str>,
}

impl<'a> Notice<'a> {
    /// A notice with the one-space convention.
    pub fn new(level: Level, glyph: &'a str, text: impl Into<Cow<'a, str>>) -> Self {
        Self {
            level,
            glyph,
            gap: 1,
            text: text.into(),
        }
    }

    /// Widen the gutter between glyph and text (the `agentic::display`
    /// two-space convention).
    #[must_use]
    pub fn gap(mut self, gap: usize) -> Self {
        self.gap = gap;
        self
    }

    /// The notice as plain text — **no ANSI, no newline**. Pure.
    ///
    /// This is the byte-for-byte contract every migrated call site is held to:
    /// the existing pure text builders (`compression_notice_text`,
    /// `retry_progress_msg`, …) must be reproducible through it exactly.
    pub fn line(&self) -> String {
        if self.glyph.is_empty() {
            return self.text.to_string();
        }
        format!("{}{}{}", self.glyph, " ".repeat(self.gap), self.text)
    }

    /// Emit the notice as a permanent line, cooperating with whatever owns the
    /// ephemeral row.
    ///
    /// - `caps` is the **ownership** gate, never `color`. A process that may
    ///   not own a terminal line does not narrate over someone's captured log;
    ///   `LineCaps::None` emits **zero bytes**. Protocol mode is an absolute
    ///   veto that an explicitly-supplied `LineCaps::Own` cannot pierce — the
    ///   same rule [`Terminal::lease_with_caps`] enforces, because fd 1 may be
    ///   a JSON-RPC wire.
    /// - `color` is **styling only**. With it off the same text is emitted
    ///   undecorated: the glyph still carries the meaning.
    /// - `sink` is explicit and defaulted nowhere — relocating a stderr notice
    ///   to stdout would break someone's `2>/dev/null`.
    pub fn emit(&self, caps: LineCaps, sink: Sink, color: bool) {
        if protocol_mode() || !caps.can_own() {
            return;
        }
        Terminal::emit_line(sink, self.writer(color));
    }

    /// The bytes this notice puts on a permanent line, as a closure the caller
    /// routes through whichever emit path it owns.
    ///
    /// Split out so the styling exists **once** while two emit paths remain
    /// necessary. [`Notice::emit`] routes through [`Terminal::emit_line`],
    /// which erases every registered ephemeral through its own gate; a writer
    /// that is *already holding* that gate — the ephemeral row's teardown,
    /// flushing a trailing line between "mark finished" and the erase — must
    /// not re-enter it, and routes the same bytes through its own lease
    /// instead. Copying the styling into that second path is how the four
    /// amber encodings this widget replaced came about in the first place.
    pub(crate) fn writer(&self, color: bool) -> impl FnOnce(&mut LineWriter<'_>) -> io::Result<()> {
        let line = self.line();
        let hue = if color { self.level.hue() } else { None };
        move |w| match hue {
            Some(h) => queue!(
                w,
                SetForegroundColor(h),
                Print(&line),
                ResetColor,
                Print("\n"),
            ),
            None => writeln!(w, "{line}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Level, Notice};

    /// Both spacing conventions render, and the composition is exactly
    /// glyph + gap + text with nothing else inserted.
    #[test]
    fn line_is_glyph_then_gap_then_text() {
        assert_eq!(Notice::new(Level::Warn, "⚠", "boom").line(), "⚠ boom");
        assert_eq!(Notice::new(Level::Ok, "✓", "done").gap(2).line(), "✓  done");
    }

    /// A text that already leads with its own marker takes an empty glyph and
    /// must NOT acquire a leading gap.
    #[test]
    fn an_empty_glyph_contributes_no_gap() {
        assert_eq!(
            Notice::new(Level::Warn, "", "⚠ already marked").line(),
            "⚠ already marked"
        );
    }

    /// `line()` is plain text. The purity test in `newt-cli`'s
    /// `stdout_purity.rs` asserts zero `\u{1b}` on a protocol wire, and every
    /// widget is upstream of that.
    #[test]
    fn line_carries_no_ansi() {
        for level in [
            Level::Info,
            Level::Ok,
            Level::Warn,
            Level::Loud,
            Level::Dim,
            Level::Debug,
        ] {
            assert!(!Notice::new(level, "⚠", "text").line().contains('\u{1b}'));
        }
    }

    /// The amber unification, pinned: the three encodings the workspace grew
    /// (`DarkYellow`, `Rgb{200,140,0}`, a raw `\x1b[33m`) collapse to ONE, and
    /// `Warn` is it.
    #[test]
    fn warn_is_the_single_amber() {
        use crossterm::style::Color as CtColor;
        assert_eq!(Level::Warn.hue(), Some(CtColor::DarkYellow));
        assert_eq!(Level::Info.hue(), None, "narration keeps the default fg");
    }
}
