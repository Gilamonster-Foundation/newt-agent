//! **Terminal-line ownership** — the arbiter every ephemeral writer registers
//! with, and the shared primitives they all draw through.
//!
//! # Why this module exists
//!
//! The workspace modelled **stdin** ownership rigorously (a process-wide
//! `Mutex` + `Condvar` + RAII guards in `newt-tui/src/permissions.rs`) and
//! modelled **stdout / terminal-line** ownership not at all. Five independent
//! spinners with four erase strategies, three frame clocks and three gating
//! predicates were the symptom; the missing output-side arbiter was the
//! disease. The user-visible cost was a permission prompt printed *onto* a live
//! spinner line and then overwritten by the next spinner frame — a question the
//! operator could not see, on a process correctly blocked in `read_line`.
//!
//! So: one place owns the ephemeral bottom line, ephemeral writers hold RAII
//! leases on it, and anything that may block on a human must first take the
//! line away from them.
//!
//! # Layout
//!
//! - [`frames`] — the ONE braille frame set and the pure line formatter.
//! - [`caps`] — [`LineCaps`], the ONE gate: may this process own a terminal
//!   line at all? Distinct from "does the user want ANSI colors".
//! - [`arbiter`] — the process singleton, its leases, and [`PromptWindow`].
//! - [`spinner`] — the ONE [`Spinner`], driven by ONE ticker thread.
//!
//! # Note on `newt-core` probing the terminal
//!
//! `newt-core/src/config.rs` records the rule *"the terminal-aware resolution
//! lives in the TUI layer — newt-core has no business probing the terminal."*
//! This module deliberately amends that rule to its narrower, still-true form:
//! **`newt-core::config` does not probe the terminal; `newt-core::tty` does, and
//! it is the only place that may.** The arbiter must be a process singleton
//! serving `newt-core::agentic`'s own spinner as well as every crate above it,
//! so `newt-core` is the only crate that can host it.

use crossterm::style::Color as CtColor;

pub mod arbiter;
pub mod caps;
pub mod frames;
mod modal;
/// D2a (#1864): the terminal renderer for `crate::progress`. Present and
/// tested, deliberately NOT wired — dual-publish keeps the old path
/// authoritative until each family's cutover PR.
pub mod progress_sink;
/// Real-resource (PTY) proof that a notice emitted from outside the lease
/// damages nothing. Unix-only: it needs a real pty pair.
#[cfg(all(test, unix))]
mod pty_notice_test;
// C2b (#1891): the ONE raw-mode guard, promoted out of `modal` when a second
// surface needed it. Saving termios rather than using crossterm's global is
// what makes nested frames compose.
pub mod raw_mode;
/// D2b (#1895): ownership of the ONE ephemeral bottom row, extracted from the
/// spinner so the renderer can own it. The concurrency rules moved verbatim.
mod row;
pub mod spinner;
pub mod widgets;
pub mod width;

pub use arbiter::{
    prompt_stdin_active, prompt_windows_constructed, try_watch_stdin, Ephemeral,
    EphemeralRegistration, LineLease, LineWriter, OnCollision, PromptWindow, Region, RegionLease,
    Sink, Terminal, WatcherStdinGuard,
};
pub use caps::{enter_protocol_mode, protocol_mode, LineCaps};
pub use frames::{format_spinner, SPINNER_FRAMES};
pub use modal::{
    modal_prompt_controls, read_prompt_window_line, ControlReader, Echo, PromptControlReader,
    PromptLine, MODAL_CONTROL_HINT, MODAL_INPUT_GLYPH,
};
pub use progress_sink::TerminalProgressSink;
pub use spinner::{interrupt_pending, set_interrupt_pending, with_spinner, Spinner};
pub use widgets::{Action, Level, Notice, Question};
pub use width::{ch_width, str_width, wrap_line};

/// The newt logo orange as a crossterm color (matches the TUI splash).
pub const NEWT_ORANGE_CT: CtColor = CtColor::Rgb {
    r: 220,
    g: 60,
    b: 20,
};

/// The high-luminance accent for whichever input surface currently owns the
/// operator's keyboard. Deliberately distinct from [`NEWT_ORANGE_CT`]: that
/// darker brand orange works for small identity marks, while an active prompt
/// must remain legible against a black terminal background.
///
/// There should be exactly one of these on screen. A modal takes the accent
/// while it owns input; the chat prompt recedes until the modal closes.
pub const ACTIVE_INPUT_CT: CtColor = CtColor::Rgb {
    r: 255,
    g: 165,
    b: 90,
};

/// Dimmer-than-DarkGrey hue for the soft "fade" tail on a truncated status
/// line — the last couple of cells before the `…` dissolve toward the
/// background so the cut reads as "there's more here", not a hard chop.
pub const FADE_CT: CtColor = CtColor::Rgb {
    r: 90,
    g: 90,
    b: 90,
};

/// Current terminal width in columns. Falls back to 80 when stdout isn't a tty
/// (headless/piped) — callers only truncate single ephemeral status lines, so a
/// conservative default is harmless.
pub fn term_cols() -> usize {
    crossterm::terminal::size()
        .map(|(c, _)| c as usize)
        .unwrap_or(80)
        .max(8)
}

/// A single status/spinner line fitted to the terminal width.
///
/// When the source overflows `max_cols` it is cut to fit with a trailing `…`,
/// and the last couple of visible cells are split off into `fade` so the caller
/// can render them dimmer (the soft fade-out). When it already fits, `fade` and
/// `ellipsis` are empty and `head` is the whole string. Width is counted in
/// **display columns** via [`width::str_width`], so a CJK or emoji label is cut
/// where the terminal actually runs out of cells.
#[derive(Debug, PartialEq, Eq)]
pub struct FittedLine {
    pub head: String,
    pub fade: String,
    pub ellipsis: &'static str,
}

/// Fit `s` into `max_cols` columns (see [`FittedLine`]).
pub fn fit_line(s: &str, max_cols: usize) -> FittedLine {
    if str_width(s) <= max_cols {
        return FittedLine {
            head: s.to_string(),
            fade: String::new(),
            ellipsis: "",
        };
    }
    // Reserve one column for the ellipsis; keep at least one visible char.
    let budget = max_cols.saturating_sub(1).max(1);
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0usize;
    for c in s.chars() {
        let cw = ch_width(c);
        if used + cw > budget {
            break;
        }
        used += cw;
        kept.push(c);
    }
    // A single glyph wider than the whole budget still shows: an empty row is
    // worse than one over-wide cell, and this row is erased in place anyway.
    if kept.is_empty() {
        kept.extend(s.chars().next());
    }
    let fade_n = 2.min(kept.len());
    let split = kept.len() - fade_n;
    FittedLine {
        head: kept[..split].iter().collect(),
        fade: kept[split..].iter().collect(),
        ellipsis: "…",
    }
}

#[cfg(test)]
mod tests {
    use super::{fit_line, term_cols};

    #[test]
    fn fit_line_passes_through_when_it_fits() {
        let f = fit_line("hello", 10);
        assert_eq!(f.head, "hello");
        assert_eq!(f.fade, "");
        assert_eq!(f.ellipsis, "");
        // Exact fit is not an overflow.
        let exact = fit_line("hello", 5);
        assert_eq!(exact.ellipsis, "");
        assert_eq!(exact.head, "hello");
    }

    #[test]
    fn fit_line_truncates_with_faded_tail_and_ellipsis() {
        // 11 chars into 6 cols: 5 visible + "…"; last 2 visible cells fade.
        let f = fit_line("abcdefghijk", 6);
        assert_eq!(f.ellipsis, "…");
        assert_eq!(f.head, "abc");
        assert_eq!(f.fade, "de");
        // Reassembled visible width (head + fade + the single ellipsis cell)
        // never exceeds the budget.
        assert!(f.head.chars().count() + f.fade.chars().count() < 6);
    }

    #[test]
    fn fit_line_handles_tiny_budgets() {
        // One column of room still yields a single visible char + ellipsis,
        // never a panic or an empty line.
        let f = fit_line("abcdef", 1);
        assert_eq!(f.ellipsis, "…");
        assert_eq!(f.head, "");
        assert_eq!(f.fade, "a");
    }

    /// The width floor keeps `fit_line`'s budget arithmetic non-degenerate even
    /// with no terminal attached (the headless / piped case).
    #[test]
    fn term_cols_never_returns_a_degenerate_width() {
        assert!(term_cols() >= 8);
    }

    /// **A deliberate behavior change** (`docs/decisions/tty_widget_suite.md`
    /// §5 row 2): `fit_line` used to count `char`s, and a `char` count is not a
    /// column count. Every ASCII case above is unaffected — this is the case
    /// that was silently wrong.
    ///
    /// `"日本語です"` is 5 `char`s but **10 columns**. Against a 6-column
    /// budget the old implementation compared `5 <= 6`, declared it a fit, and
    /// returned the whole string with no ellipsis — so the spinner painted a
    /// 10-column line onto a 6-column row, which wraps and strands a row that
    /// the single-line erase can never reach. That is the residue class the
    /// arbiter exists to eliminate, reintroduced by the measurement.
    ///
    /// The row must now fit: visible width (head + fade + the one ellipsis
    /// cell) never exceeds the budget.
    #[test]
    fn fit_line_measures_columns_so_a_cjk_label_is_actually_cut() {
        use super::width::str_width;
        let label = "日本語です";
        assert_eq!(label.chars().count(), 5, "5 chars…");
        assert_eq!(str_width(label), 10, "…but 10 columns");

        let f = fit_line(label, 6);
        assert_eq!(
            f.ellipsis, "…",
            "the old char-count result — the whole 10-column string, untruncated \
             — was wrong: it overflows a 6-column row"
        );
        assert_eq!(f.head, "");
        assert_eq!(f.fade, "日本");
        assert!(
            str_width(&f.head) + str_width(&f.fade) + str_width(f.ellipsis) <= 6,
            "the fitted row must not exceed its budget"
        );
    }

    /// The degenerate case the column measurement introduces and the `char`
    /// count could not: one glyph that is wider than the entire budget. It is
    /// still shown rather than yielding a blank row.
    #[test]
    fn fit_line_keeps_a_glyph_wider_than_the_whole_budget() {
        let f = fit_line("日本", 1);
        assert_eq!(f.head, "");
        assert_eq!(f.fade, "日");
        assert_eq!(f.ellipsis, "…");
    }
}
