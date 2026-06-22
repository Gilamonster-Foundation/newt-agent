//! Fenced-code-block highlighting seam (Step 25.6).
//!
//! `highlight(lang, body)` returns one rendered ANSI string per source line (no
//! prefix/newline — the emitter adds the container bars + indent). Without the
//! `markdown-syntect` feature it's the plain dim block (current 25.1 behavior);
//! with it, `syntect` colors each line by language. Kept behind a feature
//! because syntect's syntax + theme assets are heavy.

use super::emitter::FADE;
use super::inline::{sgr_fg, RESET};

/// Plain dim rendering — one `FADE`-colored line per source line.
#[cfg(not(feature = "markdown-syntect"))]
pub(super) fn highlight(_lang: &str, body: &str) -> Vec<String> {
    body.split('\n')
        .map(|l| format!("{}{l}{RESET}", sgr_fg(FADE)))
        .collect()
}

/// syntect-highlighted rendering — per-language token colors (foreground only,
/// so the theme background never fights the terminal). Falls back to the dim
/// block when the language is unknown or highlighting fails.
#[cfg(feature = "markdown-syntect")]
pub(super) fn highlight(lang: &str, body: &str) -> Vec<String> {
    use std::sync::OnceLock;
    use syntect::easy::HighlightLines;
    use syntect::highlighting::ThemeSet;
    use syntect::parsing::SyntaxSet;
    use syntect::util::as_24_bit_terminal_escaped;

    static ASSETS: OnceLock<(SyntaxSet, ThemeSet)> = OnceLock::new();
    let (ss, ts) = ASSETS.get_or_init(|| {
        (
            SyntaxSet::load_defaults_newlines(),
            ThemeSet::load_defaults(),
        )
    });

    let syntax = ss
        .find_syntax_by_token(lang)
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let theme = &ts.themes["base16-ocean.dark"];
    let mut h = HighlightLines::new(syntax, theme);

    body.split('\n')
        .map(|line| {
            // The "newlines" syntax set wants a trailing '\n' to terminate a
            // line; synthesize one, render, then trim it back off (the emitter
            // owns line breaks). On any error, fall back to a dim line.
            let with_nl = format!("{line}\n");
            match h.highlight_line(&with_nl, ss) {
                Ok(ranges) => {
                    let escaped = as_24_bit_terminal_escaped(&ranges, false);
                    format!("{}{RESET}", escaped.trim_end_matches('\n'))
                }
                Err(_) => format!("{}{line}{RESET}", sgr_fg(FADE)),
            }
        })
        .collect()
}
