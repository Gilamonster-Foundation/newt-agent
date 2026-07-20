//! The ONE spinner frame set and the ONE ephemeral-line formatter.
//!
//! Both were duplicated across three crates before this module existed
//! (`newt-core/src/agentic/mod.rs`, `newt-tui/src/setup_tui.rs`, and a
//! hardcoded frame-0 glyph in `newt-tui/src/lib.rs`) — not by design, but
//! because `agentic::display` was a private module with a curated re-export
//! list, so nothing outside `agentic` could import them. They live here, in a
//! public module, so the duplication has nowhere to come back from.

/// Spinner glyph frames (braille) for every ephemeral liveness line in the
/// workspace. Seeded verbatim from the pre-unification
/// `agentic::mod::SPINNER_FRAMES`, so the visible animation is unchanged.
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The ephemeral spinner line text. Pure for testing. A braille spinner that
/// advances every frame (so the line is visibly alive even while only the clock
/// moves), a stage `label` telling the user what's happening right now
/// (`thinking…`, `compressing context…`, …), and the elapsed seconds. The
/// `· N chars` tail is shown only once generation has produced output
/// (`chars > 0`).
pub fn format_spinner(frame: usize, secs: f32, label: &str, chars: usize) -> String {
    let braille = SPINNER_FRAMES[frame % SPINNER_FRAMES.len()];
    if chars == 0 {
        format!("{braille} {label} {secs:.1}s")
    } else {
        format!("{braille} {label} {secs:.1}s · {chars} chars")
    }
}

#[cfg(test)]
mod tests {
    use super::{format_spinner, SPINNER_FRAMES};

    /// Ported verbatim from `agentic::mod`'s `spinner_line_formats_and_frame_wraps`
    /// so the formatter's contract survives the move.
    #[test]
    fn spinner_line_formats_and_frame_wraps() {
        // Braille spinner + stage label + clock; the chars tail shows once
        // generation has produced output.
        assert_eq!(
            format_spinner(0, 1.23, "thinking…", 340),
            "⠋ thinking… 1.2s · 340 chars"
        );
        // A different stage label, and chars == 0 drops the `· N chars` tail.
        assert_eq!(
            format_spinner(1, 0.5, "compressing context…", 0),
            "⠙ compressing context… 0.5s"
        );
        // Frame index wraps over the braille glyph set.
        assert!(
            format_spinner(SPINNER_FRAMES.len(), 0.0, "thinking…", 0).contains(SPINNER_FRAMES[0])
        );
    }

    /// The frame set is the braille run the whole workspace shares. Pinned so a
    /// future edit here is a deliberate visual change, not a drift.
    #[test]
    fn frame_set_is_the_ten_braille_glyphs() {
        assert_eq!(SPINNER_FRAMES.len(), 10);
        assert_eq!(SPINNER_FRAMES[0], "⠋");
        assert!(
            SPINNER_FRAMES.iter().all(|f| f.chars().count() == 1
                && ('\u{2800}'..='\u{28FF}')
                    .contains(&f.chars().next().expect("one glyph per frame"))),
            "every frame is a single braille glyph: {SPINNER_FRAMES:?}"
        );
    }
}
