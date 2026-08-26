//! **The Newt Markup Markdown dialect — named, documented, and pinned.**
//!
//! Before A1 (epic #1803, #1825) the two Markdown parser sites each chose
//! their own undocumented option matrix inline. The matrices themselves are
//! DELIBERATELY different today — the A0 inventory froze that divergence,
//! and C3's deletion gate ("remove the second independent Markdown option
//! matrix") owns unifying them — but the choice is no longer undocumented:
//! both sites consume the named constants below, and the tests here pin
//! them bit-for-bit.
//!
//! ## The dialect, specified
//!
//! - **Encoding:** UTF-8 in, UTF-8 out (`&str` end to end). Line endings are
//!   not normalized by Newt; pulldown-cmark treats `\r\n` per CommonMark.
//! - **Canonical (plain/TUI) extensions:** GFM strikethrough, task lists,
//!   and tables over CommonMark — nothing else. No smart punctuation (byte
//!   fidelity for committed output), no footnotes, no heading attributes.
//! - **Raw HTML:** parsed as `Event::Html` per CommonMark; the ANSI emitter
//!   renders it as plain text (it styles, it does not execute), and the web
//!   renderer sanitizes everything through `ammonia` — raw HTML never
//!   reaches a page unfiltered.
//! - **Soft breaks:** the canonical dialect leaves them to the renderer
//!   (CommonMark folds them); the web view maps soft breaks to hard breaks
//!   so chat text keeps its newlines (`newt-web/src/shell.rs`).
//! - **Fenced code:** never interpreted — a fence's info string selects
//!   syntax presentation only. The `mermaid` info string is a web-side
//!   progressive enhancement (`shell.rs`); E0 will move it behind the
//!   extension registry with mandatory source fallback.
//! - **Unknown extensions:** there is no inline-extension syntax in v1;
//!   unknown fenced-block info strings fall back to plain code
//!   presentation, visibly (ADR law 5).
//!
//! ## The web-enhancement matrix
//!
//! `newt-web` parses with every pulldown-cmark extension enabled
//! ([`Options::all`]) to match the Scrybe toolchain's rendering, then
//! sanitizes. That is a RENDERING-SIDE superset, not a second semantic
//! dialect: the document model other surfaces see is the canonical set.

use pulldown_cmark::Options;

/// The canonical Newt Markup dialect: CommonMark + GFM strikethrough, task
/// lists, and tables. The plain/TUI renderer parses with exactly this set.
#[must_use]
pub fn canonical_options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_TABLES);
    options
}

/// The web view's rendering-side superset (every extension, then ammonia
/// sanitation and soft→hard breaks). Documented divergence — C3 owns
/// removing the second matrix.
#[must_use]
pub fn web_enhancement_options() -> Options {
    Options::all()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical set is EXACTLY the frozen pre-A1 TUI set — bit for bit.
    /// Adding an extension here is a dialect change: it alters how every
    /// surface reads every document, and belongs to its own reviewed slice.
    #[test]
    fn canonical_options_are_exactly_the_frozen_tui_set() {
        let expected =
            Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES;
        assert_eq!(canonical_options(), expected);
        // Spelled out negatively for the extensions most likely to creep in.
        for absent in [
            Options::ENABLE_SMART_PUNCTUATION,
            Options::ENABLE_FOOTNOTES,
            Options::ENABLE_HEADING_ATTRIBUTES,
        ] {
            assert!(
                !canonical_options().contains(absent),
                "{absent:?} is not part of the canonical dialect"
            );
        }
    }

    #[test]
    fn web_enhancement_matrix_is_documented_options_all() {
        assert_eq!(web_enhancement_options(), Options::all());
        assert!(
            web_enhancement_options().contains(canonical_options()),
            "the web superset must contain the canonical dialect"
        );
    }
}
