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
pub mod spinner;
pub mod width;

pub use arbiter::{
    prompt_stdin_active, prompt_windows_constructed, try_watch_stdin, Ephemeral, LineLease,
    LineWriter, PromptWindow, Sink, Terminal, WatcherStdinGuard,
};
pub use caps::{enter_protocol_mode, protocol_mode, LineCaps};
pub use frames::{format_spinner, SPINNER_FRAMES};
pub use spinner::{with_spinner, Spinner};
pub use width::{ch_width, str_width, wrap_line};

/// The newt logo orange as a crossterm color (matches the TUI splash).
pub const NEWT_ORANGE_CT: CtColor = CtColor::Rgb {
    r: 220,
    g: 60,
    b: 20,
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
/// `char`s — good enough for the braille spinner + ASCII status text these
/// lines carry; no CJK in this path.
#[derive(Debug, PartialEq, Eq)]
pub struct FittedLine {
    pub head: String,
    pub fade: String,
    pub ellipsis: &'static str,
}

/// Fit `s` into `max_cols` columns (see [`FittedLine`]).
pub fn fit_line(s: &str, max_cols: usize) -> FittedLine {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_cols {
        return FittedLine {
            head: s.to_string(),
            fade: String::new(),
            ellipsis: "",
        };
    }
    // Reserve one column for the ellipsis; keep at least one visible char.
    let budget = max_cols.saturating_sub(1).max(1);
    let kept = &chars[..budget];
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
}
