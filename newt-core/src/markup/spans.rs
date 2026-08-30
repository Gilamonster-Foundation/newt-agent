//! **A renderer-neutral span projection of canonical Markdown** (C2 of epic
//! #1803, #1876).
//!
//! [`plain::render`](super::plain::render) projects a definition to lines of
//! text. That is the canonical fallback and every surface can print it. A
//! RichTUI can do better — bold headings, dim code, a highlighted option —
//! but only if it knows which run of characters means what.
//!
//! This module answers that, and answers it **without naming a renderer**. It
//! emits [`SpanLine`]s of [`Span`]s carrying an [`Emphasis`] role; mapping a
//! role onto `ratatui::Style`, an ANSI sequence, or a CSS class belongs to
//! whichever view is doing the drawing.
//!
//! # Why it lives in `newt-core` and not in `newt-tui`
//!
//! Because the parser does. C3a (#1857) ratcheted the repo's Markdown parser
//! constructors from two to **one**: surfaces no longer choose a dialect,
//! they call [`dialect::parse`](super::dialect::parse). A RichTUI that
//! consumed `pulldown_cmark::Event` directly would need pulldown as a
//! dependency of the TUI crate and would put dialect knowledge back in a
//! surface — the divergence C3a deleted.
//!
//! So the projection happens where the parser already is, and `newt-tui`
//! receives plain data. CLAUDE.md's rule for exactly this: *shared
//! functionality moves down into the minimal layer.* The consequence worth
//! stating is the one that keeps the boundary honest in both directions —
//! **no `ratatui` type appears here, and no `pulldown_cmark` type escapes
//! here.** [`Emphasis`] is a closed enum of meanings, not of styles.
//!
//! # No wrapping
//!
//! Logical lines only. Wrapping is the view's, because only the view knows
//! its width — and `ratatui::widgets::Paragraph` already wraps styled lines,
//! so a wrap implementation here would be a second width model for D3 to
//! delete later.

use super::dialect;
use pulldown_cmark::{Event, Tag, TagEnd};

/// What a run of characters MEANS. Not how it looks.
///
/// Closed deliberately: an open vocabulary would let a document demand a
/// presentation this build has never heard of, and ADR law 5 wants unknown
/// content to fall back visibly rather than to be passed through. Anything
/// the canonical dialect can express that is not listed here arrives as
/// [`Emphasis::Plain`] — visible, unstyled, never dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Emphasis {
    /// Ordinary body text.
    #[default]
    Plain,
    /// `**strong**`.
    Strong,
    /// `*emphasis*`.
    Emphasis,
    /// `` `code` `` and fenced-block content.
    Code,
    /// A heading's text, at `level` (1-6).
    Heading(u8),
    /// `~~struck~~`.
    Struck,
    /// Blockquote content.
    Quote,
    /// A list item's bullet or number — the marker, not the content.
    Marker,
}

/// One styled run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// The characters. Never empty in an emitted line.
    pub text: String,
    /// What they mean.
    pub emphasis: Emphasis,
}

impl Span {
    /// A run of plain body text.
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: Self::displayable(text),
            emphasis: Emphasis::Plain,
        }
    }

    /// A run carrying a meaning.
    #[must_use]
    pub fn styled(text: impl Into<String>, emphasis: Emphasis) -> Self {
        Self {
            text: Self::displayable(text),
            emphasis,
        }
    }

    /// **Every span is display text, so every span is neutralised** (#1941).
    ///
    /// In the constructors rather than at the call sites, because the call
    /// sites are where this was already missed once: neutralising
    /// `spans::project`'s *input* covered the markdown body and nothing else,
    /// while `interaction_view` builds an option label, a note line, and a
    /// field label into spans DIRECTLY — and an option label is the `allow` /
    /// `deny` text a permission prompt spoofs. Making the type carry the rule
    /// is what the reuse discipline means by preferring an unrepresentable bug
    /// to a fix at N sites.
    ///
    /// Idempotent: the marker `<U+202E>` contains no hazard of its own, so the
    /// body — neutralised once on the way into the parser and again here — is
    /// unchanged by the second pass. `project` still neutralises its input
    /// because its internal run-merging appends to `text` directly and does
    /// not come back through these constructors.
    fn displayable(text: impl Into<String>) -> String {
        match crate::notes_scan::neutralize_for_display(&text.into()) {
            std::borrow::Cow::Borrowed(clean) => clean.to_string(),
            std::borrow::Cow::Owned(fixed) => fixed,
        }
    }
}

/// One logical line. Empty (`spans.is_empty()`) is a blank line, which is
/// meaningful: it separates blocks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpanLine {
    pub spans: Vec<Span>,
}

impl SpanLine {
    /// The line's characters, with every role dropped.
    ///
    /// The bridge back to the canonical projection: a view that cannot style
    /// can still print this, and it must equal what a plain surface would
    /// show. `c2::flattening_a_projection_loses_styling_and_nothing_else`
    /// holds that.
    #[must_use]
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.spans.iter().all(|s| s.text.trim().is_empty())
    }
}

/// Project canonical Markdown into styled logical lines.
///
/// Uses [`dialect::parse`] — the repo's ONE parser constructor — so this adds
/// no dialect and no second option matrix.
#[must_use]
pub fn project(markdown: &str) -> Vec<SpanLine> {
    // #1941: before parsing, not after. A hazard neutralised per-span would
    // have to be caught at every `push` site; done here it cannot be missed,
    // and the marker text flows through the parser as ordinary characters.
    let markdown = &*crate::notes_scan::neutralize_for_display(markdown);
    let mut out: Vec<SpanLine> = Vec::new();
    let mut line = SpanLine::default();
    let mut stack: Vec<Emphasis> = Vec::new();
    let mut in_code_block = false;
    let mut list_depth: usize = 0;

    // The innermost meaning wins: `**bold `code`**` is code where the code
    // is, because that is the more specific claim about those characters.
    let current = |stack: &[Emphasis]| stack.last().copied().unwrap_or_default();

    let push = |line: &mut SpanLine, text: &str, emphasis: Emphasis| {
        if text.is_empty() {
            return;
        }
        // Merge with the previous run when the meaning has not changed, so a
        // view is not handed three adjacent spans that style identically.
        match line.spans.last_mut() {
            Some(prev) if prev.emphasis == emphasis => prev.text.push_str(text),
            _ => line.spans.push(Span::styled(text, emphasis)),
        }
    };

    let break_line = |line: &mut SpanLine, out: &mut Vec<SpanLine>| {
        out.push(std::mem::take(line));
    };

    for event in dialect::parse(markdown) {
        match event {
            Event::Start(Tag::Strong) => stack.push(Emphasis::Strong),
            Event::Start(Tag::Emphasis) => stack.push(Emphasis::Emphasis),
            Event::Start(Tag::Strikethrough) => stack.push(Emphasis::Struck),
            Event::Start(Tag::BlockQuote(_)) => stack.push(Emphasis::Quote),
            Event::Start(Tag::Heading { level, .. }) => {
                stack.push(Emphasis::Heading(level as u8));
            }
            Event::Start(Tag::CodeBlock(_)) => {
                in_code_block = true;
                stack.push(Emphasis::Code);
            }
            Event::Start(Tag::List(_)) => list_depth += 1,
            Event::Start(Tag::Item) => {
                let indent = "  ".repeat(list_depth.saturating_sub(1));
                push(&mut line, &format!("{indent}• "), Emphasis::Marker);
            }
            Event::End(
                TagEnd::Strong
                | TagEnd::Emphasis
                | TagEnd::Strikethrough
                | TagEnd::BlockQuote(_)
                | TagEnd::Heading(_),
            ) => {
                stack.pop();
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                stack.pop();
                break_line(&mut line, &mut out);
            }
            Event::End(TagEnd::List(_)) => list_depth = list_depth.saturating_sub(1),
            Event::End(TagEnd::Item) => break_line(&mut line, &mut out),
            Event::End(TagEnd::Paragraph) => break_line(&mut line, &mut out),
            Event::Text(text) => {
                let emphasis = current(&stack);
                if in_code_block {
                    // A fenced block's newlines are real line breaks, not
                    // soft wrapping: a code block that reflowed would be a
                    // different program.
                    let mut parts = text.split('\n').peekable();
                    while let Some(part) = parts.next() {
                        push(&mut line, part, emphasis);
                        if parts.peek().is_some() {
                            break_line(&mut line, &mut out);
                        }
                    }
                } else {
                    push(&mut line, &text, emphasis);
                }
            }
            Event::Code(code) => push(&mut line, &code, Emphasis::Code),
            // The canonical dialect folds soft breaks (C0a froze that); a
            // hard break is a real new line.
            Event::SoftBreak => push(&mut line, " ", current(&stack)),
            Event::HardBreak => break_line(&mut line, &mut out),
            // Raw HTML is rendered as literal visible text, never
            // interpreted — the canonical dialect's rule, restated here so
            // the rich view cannot quietly differ from the plain one.
            Event::Html(html) | Event::InlineHtml(html) => {
                push(&mut line, &html, Emphasis::Plain);
            }
            Event::Rule => {
                break_line(&mut line, &mut out);
                push(&mut line, "───", Emphasis::Marker);
                break_line(&mut line, &mut out);
            }
            Event::TaskListMarker(done) => {
                push(
                    &mut line,
                    if done { "[x] " } else { "[ ] " },
                    Emphasis::Marker,
                );
            }
            // Everything else the canonical dialect can produce (table
            // structure, footnotes we do not enable, image/link wrappers)
            // contributes no styling of its own; its TEXT still arrives
            // through `Event::Text`, so nothing is dropped.
            _ => {}
        }
    }
    if !line.spans.is_empty() {
        out.push(line);
    }
    // A trailing blank from a block close is structure, not content.
    while out.last().is_some_and(SpanLine::is_blank) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod c2 {
    use super::*;

    fn flat(markdown: &str) -> Vec<String> {
        project(markdown).iter().map(SpanLine::text).collect()
    }

    fn roles(markdown: &str) -> Vec<Vec<(String, Emphasis)>> {
        project(markdown)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| (s.text.clone(), s.emphasis))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn plain_text_projects_to_one_plain_line() {
        assert_eq!(
            roles("hello world"),
            vec![vec![("hello world".to_string(), Emphasis::Plain)]]
        );
    }

    #[test]
    fn the_inline_vocabulary_carries_meaning_not_style() {
        assert_eq!(
            roles("**bold** and *soft* and `code` and ~~gone~~"),
            vec![vec![
                ("bold".to_string(), Emphasis::Strong),
                (" and ".to_string(), Emphasis::Plain),
                ("soft".to_string(), Emphasis::Emphasis),
                (" and ".to_string(), Emphasis::Plain),
                ("code".to_string(), Emphasis::Code),
                (" and ".to_string(), Emphasis::Plain),
                ("gone".to_string(), Emphasis::Struck),
            ]]
        );
    }

    #[test]
    fn a_heading_carries_its_level() {
        assert_eq!(
            roles("### three"),
            vec![vec![("three".to_string(), Emphasis::Heading(3))]]
        );
    }

    #[test]
    fn the_innermost_meaning_wins() {
        // `**bold `code`**` is code where the code is: the more specific
        // claim about those characters.
        let r = roles("**bold `code`**");
        assert_eq!(r[0][0], ("bold ".to_string(), Emphasis::Strong));
        assert_eq!(r[0][1], ("code".to_string(), Emphasis::Code));
    }

    #[test]
    fn adjacent_runs_of_one_meaning_merge() {
        // A view handed three adjacent spans that style identically would
        // draw three times for no reason.
        let line = &project("plain *a* more")[0];
        assert_eq!(line.spans.len(), 3, "{:?}", line.spans);
    }

    #[test]
    fn a_code_block_keeps_its_line_breaks() {
        // A fenced block that reflowed would be a different program.
        assert_eq!(
            flat("```\nfn a() {}\nfn b() {}\n```"),
            vec!["fn a() {}".to_string(), "fn b() {}".to_string()]
        );
        let r = roles("```\nfn a() {}\n```");
        assert_eq!(r[0][0].1, Emphasis::Code);
    }

    #[test]
    fn a_soft_break_folds_and_a_hard_break_does_not() {
        // The canonical dialect folds soft breaks; C0a froze that.
        assert_eq!(flat("one\ntwo"), vec!["one two".to_string()]);
        assert_eq!(
            flat("one  \ntwo"),
            vec!["one".to_string(), "two".to_string()]
        );
    }

    #[test]
    fn list_items_get_a_marker_and_their_own_lines() {
        let r = roles("- first\n- second");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0][0], ("• ".to_string(), Emphasis::Marker));
        assert_eq!(r[0][1], ("first".to_string(), Emphasis::Plain));
    }

    #[test]
    fn a_task_list_marker_is_visible() {
        let f = flat("- [x] done\n- [ ] todo");
        assert_eq!(f[0], "• [x] done");
        assert_eq!(f[1], "• [ ] todo");
    }

    #[test]
    fn raw_html_is_literal_text_never_interpreted() {
        // The canonical dialect's rule. A rich view that interpreted it
        // would differ from the plain one, which is the divergence C3a
        // deleted for the web.
        let f = flat("a <b>tag</b> here");
        assert!(f[0].contains("<b>"), "{f:?}");
        assert!(f[0].contains("</b>"), "{f:?}");
    }

    /// **The projection loses styling and nothing else.**
    ///
    /// The bridge to the canonical plain form: a view that cannot style must
    /// be able to print `SpanLine::text()` and show what a plain surface
    /// shows. If this drifted, RichTUI would become a second source of
    /// truth — the thing constraint 7 forbids.
    #[test]
    fn flattening_a_projection_loses_styling_and_nothing_else() {
        for markdown in [
            "plain words",
            "**bold** and `code`",
            "### heading",
            "- a\n- b",
            "> quoted",
        ] {
            let flattened = flat(markdown).join("\n");
            for word in markdown
                .split_whitespace()
                .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
                .filter(|w| !w.is_empty())
            {
                assert!(
                    flattened.contains(word),
                    "{word:?} vanished from {markdown:?} -> {flattened:?}"
                );
            }
        }
    }

    /// **Anti-vacuous twin.** The check above is a containment assertion, so
    /// it would pass on a projection that dropped STYLING information while
    /// keeping text — and equally on one that kept everything. This proves
    /// the projection carries roles at all, so "nothing else is lost" is a
    /// claim about a projection that actually distinguishes runs.
    #[test]
    fn the_projection_would_notice_a_role_that_stopped_being_carried() {
        let styled = project("**bold**");
        assert_eq!(styled[0].spans[0].emphasis, Emphasis::Strong);
        // ...and a document with no markup produces the default role, so the
        // assertion above is discriminating rather than always-true.
        let unstyled = project("bold");
        assert_eq!(unstyled[0].spans[0].emphasis, Emphasis::Plain);
        assert_ne!(styled[0].spans[0].emphasis, unstyled[0].spans[0].emphasis);
    }

    /// No `ratatui` type appears in this module, and no `pulldown_cmark`
    /// type escapes it. Both directions matter: the first keeps a renderer
    /// out of the core, the second keeps dialect knowledge out of surfaces.
    #[test]
    fn the_projection_names_no_renderer_and_leaks_no_parser_type() {
        let source = include_str!("spans.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("the production half");
        let code: String = production
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains("ratatui"),
            "a renderer type reached the core projection"
        );
        // pulldown is USED here (that is the point) but must not appear in
        // any public signature — the `pub fn`/`pub struct`/`pub enum` lines.
        for line in code.lines().filter(|l| l.trim_start().starts_with("pub ")) {
            assert!(
                !line.contains("Event") && !line.contains("Parser") && !line.contains("Tag"),
                "a pulldown type escaped through a public signature: {line}"
            );
        }
        // ...and the scan reads real code.
        assert!(code.contains("dialect::parse"), "the scan read nothing");
    }
}
