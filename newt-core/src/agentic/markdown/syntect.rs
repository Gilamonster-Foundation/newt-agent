//! Fenced-code-block highlighting seam (Step 25.6).
//!
//! `highlight(lang, body)` returns one rendered ANSI string per source line (no
//! prefix/newline — the emitter adds the container bars + indent). Without the
//! `markdown-syntect` feature it's the plain dim block (current 25.1 behavior);
//! with it, `syntect` colors each line by language. Kept behind a feature
//! because syntect's syntax + theme assets are heavy.

use super::emitter::fade;
use super::inline::RESET;

/// Plain dim rendering — one `FADE`-colored line per source line.
#[cfg(not(feature = "markdown-syntect"))]
pub(super) fn highlight(_lang: &str, body: &str) -> Vec<String> {
    body.split('\n')
        .map(|l| format!("{}{l}{RESET}", fade()))
        .collect()
}

/// syntect-highlighted rendering — per-language token colors (foreground only,
/// so the theme background never fights the terminal). Falls back to the dim
/// block when the language is unknown or highlighting fails.
#[cfg(feature = "markdown-syntect")]
pub(super) fn highlight(lang: &str, body: &str) -> Vec<String> {
    use syntect::util::as_24_bit_terminal_escaped;

    let highlights = super::super::syntax_foreground::highlight(lang, body);
    body.split('\n')
        .zip(highlights)
        .map(|(line, highlighted)| {
            // The "newlines" syntax set wants a trailing '\n' to terminate a
            // line; synthesize one, render, then trim it back off (the emitter
            // owns line breaks). On any error, fall back to a dim line.
            let with_nl = format!("{line}\n");
            match highlighted {
                Some(ranges) => {
                    let ranges: Vec<_> = ranges
                        .iter()
                        .map(|run| (run.style, &with_nl[run.bytes.clone()]))
                        .collect();
                    let escaped = as_24_bit_terminal_escaped(&ranges, false);
                    format!("{}{RESET}", escaped.trim_end_matches('\n'))
                }
                None => format!("{}{line}{RESET}", fade()),
            }
        })
        .collect()
}
