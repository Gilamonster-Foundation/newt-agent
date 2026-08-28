//! **The Newt Markup Markdown dialect — one matrix, one constructor.**
//!
//! Before A1 (epic #1803, #1825) the two Markdown parser sites each chose
//! their own undocumented option matrix inline. A1 named both but did not
//! unify them. **C3a (#1857) unified them: there is now exactly one
//! dialect, and no call site can choose otherwise** — [`parse`] is the only
//! way to obtain a parser, so a second matrix is unrepresentable rather
//! than merely discouraged.
//!
//! ## Which side moved, and why
//!
//! The **web narrowed to the canonical set**; `web_enhancement_options`
//! (`Options::all`) is deleted. Four reasons, in order of force:
//!
//! 1. **`Options::all` contained a second implementation of Newt Markup's
//!    own envelope.** `ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS` makes
//!    pulldown treat a leading `+++` block as metadata, and `push_html`
//!    writes metadata blocks as *nothing*. So the epic's own front-matter
//!    format vanished silently from the web transcript while the terminal
//!    showed it — a second grammar for the envelope
//!    [`crate::markup::strip_newt_metadata`] already owns, disagreeing with
//!    it, invisibly. `ENABLE_YAML_STYLE_METADATA_BLOCKS` did the same for
//!    `---`. Deleting the matrix deletes both.
//! 2. **Byte fidelity.** A transcript is re-sent to the model verbatim.
//!    `ENABLE_SMART_PUNCTUATION` rewrote `"` into `“`, `--` into `–`, and
//!    `...` into `…` in the view only, so the operator read punctuation the
//!    model never emitted and will never see.
//! 3. **Law 5 — unknown content falls back *visibly*.** Under the wide
//!    matrix, `# Title {#anchor}` lost `{#anchor}` (consumed into an `id`
//!    the sanitizer then stripped) and `$x^2$` lost its delimiters. Both
//!    are now plain, readable text on both surfaces.
//! 4. **The other direction was not available.** Widening the canonical set
//!    changes what every surface reads in every document and moves A0's
//!    frozen terminal goldens; that is a dialect change and belongs to its
//!    own reviewed slice, not to a unification.
//!
//! What the web gives up is real and worth naming: footnotes, definition
//! lists, math, heading anchors, and GFM alerts no longer get special
//! rendering there. Each now renders as the literal Markdown that produced
//! it — which is what the terminal has always shown.
//!
//! ## The dialect, specified
//!
//! - **Encoding:** UTF-8 in, UTF-8 out (`&str` end to end). Line endings are
//!   not normalized by Newt; pulldown-cmark treats `\r\n` per CommonMark.
//! - **Extensions:** GFM strikethrough, task lists, and tables over
//!   CommonMark — nothing else, on every surface. No smart punctuation, no
//!   footnotes, no heading attributes, no metadata blocks (the `+++`
//!   envelope is [`crate::markup`]'s, not the Markdown parser's).
//! - **Raw HTML:** parsed as `Event::Html` per CommonMark. What a view does
//!   with it is a rendering decision, not a dialect one — see below.
//! - **Fenced code:** never interpreted — a fence's info string selects
//!   syntax presentation only. The `mermaid` info string is a web-side
//!   progressive enhancement (`newt-web/src/shell.rs`); E0 will move it
//!   behind the extension registry with mandatory source fallback.
//! - **Unknown extensions:** there is no inline-extension syntax in v1;
//!   unknown fenced-block info strings fall back to plain code
//!   presentation, visibly (ADR law 5).
//!
//! ## The two sanctioned rendering-side divergences
//!
//! One dialect, two media. These are **not** dialect forks — both surfaces
//! see the same events and disagree only about how to draw them — and they
//! are exhaustive. `newt-web`'s `shell::tests::c3a` pins both, so a third
//! difference cannot appear on the excuse that the web already differs.
//!
//! 1. **Soft breaks.** CommonMark folds them. The ANSI scroller folds to a
//!    space; the web emits `<br>`, because chat text uses single newlines
//!    meaningfully and a page has no other way to keep them.
//! 2. **Raw HTML.** The scroller has no DOM, so it prints markup as literal
//!    text and executes nothing. A page *has* a DOM, so it must remove what
//!    it will not execute — everything is allowlist-sanitized through
//!    `ammonia`. Same event, opposite correct handling.

use pulldown_cmark::{Options, Parser};

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

/// Parse `src` in the Newt Markup dialect.
///
/// **The only parser constructor in the repository.** Options are not a
/// parameter, because a call site that can choose them is a call site that
/// can fork the dialect — which is exactly what happened before C3a, twice,
/// undocumented. Prefer making the bug unrepresentable over catching it in
/// review; `markup_sprawl_ratchet::c3a` holds the line mechanically.
#[must_use]
pub fn parse(src: &str) -> Parser<'_> {
    Parser::new_ext(src, canonical_options())
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

    /// **C3a (#1857): the parser this module hands out uses that set.**
    ///
    /// The ratchet proves nothing else constructs a parser; this proves the
    /// one that does is on the canonical matrix, so the two guards together
    /// mean "one dialect" rather than "one function call".
    #[test]
    fn parse_uses_the_canonical_matrix_and_nothing_wider() {
        use pulldown_cmark::Event;

        // A canonical extension is live…
        let struck: Vec<_> = parse("~~x~~").collect();
        assert!(
            struck.iter().any(|e| matches!(e, Event::Start(_))),
            "strikethrough must parse: {struck:?}"
        );

        // …and an extension outside the set is inert: `$x$` stays literal
        // text rather than becoming an InlineMath event, and a leading
        // `+++` block stays visible rather than being swallowed as
        // metadata. The second is the one that matters — the envelope
        // belongs to `crate::markup`, not to pulldown.
        let math: Vec<_> = parse("$x^2$").collect();
        assert!(
            !math.iter().any(|e| matches!(e, Event::InlineMath(_))),
            "math is outside the dialect: {math:?}"
        );
        let envelope = parse("+++\ntitle = \"kept\"\n+++\n\nbody\n")
            .filter_map(|e| match e {
                Event::Text(t) => Some(t.to_string()),
                _ => None,
            })
            .collect::<String>();
        assert!(
            envelope.contains("kept"),
            "a `+++` block must reach the renderer as visible text, not be \
             consumed as a pulldown metadata block: {envelope:?}"
        );
    }
}
