//! The block-level event walker: pulldown-cmark `Event`s → an ANSI string of
//! scrolled lines.
//!
//! Design notes (Step 24.1):
//! - **No repaint.** Output is append-only scrolled text; once a physical line
//!   is written it is final. Wrapping therefore happens *before* emit, at
//!   completed-line granularity.
//! - **Container prefixes.** Blockquote bars (`│ `, dim) and list indents are
//!   computed from the container stacks and prepended to every physical line of
//!   a block; a list item's marker (`• ` / `N. `) decorates only the first
//!   physical line via `pending_first`.
//! - **Out of scope here (later steps):** streaming (24.3 wraps this), GFM
//!   tables (24.2), syntax highlighting (24.6). Tables are not enabled in the
//!   parser yet, so pipe rows render as ordinary paragraphs for now.

use super::inline::{render_cells, sgr_fg, wrap_cells, Cell, Style, RESET};
use super::width::str_width;
use crossterm::style::Color as CtColor;
use pulldown_cmark::{Event, Tag, TagEnd};

/// Dim "fade" hue — reused for blockquote bars, code blocks, rules, inline
/// code, and link URLs. Single source of truth with the rest of the TUI.
pub(super) const FADE: CtColor = crate::agentic::display::FADE_CT;
/// The newt logo orange — headings.
const ORANGE: CtColor = crate::agentic::display::NEWT_ORANGE_CT;

/// Accumulating renderer. One per `render_markdown` call.
pub(super) struct Emitter {
    out: String,
    cols: usize,

    /// Cells of the current, not-yet-flushed logical line.
    cur_cells: Vec<Cell>,
    /// Completed logical lines of the current block (split at hard breaks).
    block_lines: Vec<Vec<Cell>>,

    // Inline style nesting depths (a counter, so `**a _b_ c**` nests cleanly).
    bold: u32,
    italic: u32,
    strike: u32,
    link: u32,
    heading: u32,

    // Container state.
    quote_depth: usize,
    /// One slot per open list: `Some(n)` = ordered, next number; `None` = bullet.
    lists: Vec<Option<u64>>,
    /// Continuation indent for the current container's content.
    indent: String,
    /// Saved `indent`s, restored on `End(Item)`.
    indent_stack: Vec<String>,
    /// Marker prefix for the first physical line of the current item.
    pending_first: Option<String>,

    // Fenced/indented code block accumulation.
    in_code: bool,
    code_buf: String,

    /// Destination of the currently-open link, appended dimly on `End(Link)`.
    link_url: Option<String>,
}

impl Emitter {
    pub(super) fn new(cols: usize) -> Self {
        Self {
            out: String::new(),
            cols: cols.max(8),
            cur_cells: Vec::new(),
            block_lines: Vec::new(),
            bold: 0,
            italic: 0,
            strike: 0,
            link: 0,
            heading: 0,
            quote_depth: 0,
            lists: Vec::new(),
            indent: String::new(),
            indent_stack: Vec::new(),
            pending_first: None,
            in_code: false,
            code_buf: String::new(),
            link_url: None,
        }
    }

    /// Drive one parser event.
    pub(super) fn handle(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(s) => {
                if self.in_code {
                    self.code_buf.push_str(&s);
                } else {
                    let style = self.cur_style();
                    self.push_text(&s, style);
                }
            }
            Event::Code(s) => {
                let mut style = self.cur_style();
                style.code = true;
                self.push_text(&s, style);
            }
            // Render raw HTML as literal text (no DOM in a scroller). The
            // guards skip these inside a code block, falling through to `_`.
            Event::Html(s) | Event::InlineHtml(s) if !self.in_code => {
                let style = self.cur_style();
                self.push_text(&s, style);
            }
            Event::SoftBreak if !self.in_code => {
                self.push_text(" ", Style::default());
            }
            Event::HardBreak if !self.in_code => {
                self.block_lines.push(std::mem::take(&mut self.cur_cells));
            }
            Event::Rule => self.emit_rule(),
            Event::TaskListMarker(checked) => {
                let glyph = if checked { "✓ " } else { "☐ " };
                self.push_text(glyph, Style::default());
            }
            _ => {}
        }
    }

    /// Finish the document: flush any trailing block and drop a single trailing
    /// newline so the caller controls the final line break.
    pub(super) fn finish(mut self) -> String {
        if !self.cur_cells.is_empty() || !self.block_lines.is_empty() {
            self.emit_block();
        }
        if self.out.ends_with('\n') {
            self.out.pop();
        }
        self.out
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { .. } => self.heading += 1,
            Tag::BlockQuote(_) => {
                // A blockquote nested inside an item must not absorb that item's
                // still-buffered text — flush it first (tight-list case).
                self.flush_inline();
                self.block_break_if_top();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(_) => {
                self.flush_inline();
                self.in_code = true;
                self.code_buf.clear();
            }
            Tag::List(start) => {
                self.flush_inline();
                self.block_break_if_top();
                self.lists.push(start);
            }
            Tag::Item => self.start_item(),
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => {
                self.link += 1;
                self.link_url = Some(dest_url.to_string());
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.emit_block(),
            TagEnd::Heading(_) => {
                self.emit_block();
                self.heading = self.heading.saturating_sub(1);
            }
            TagEnd::BlockQuote(_) => self.quote_depth = self.quote_depth.saturating_sub(1),
            TagEnd::CodeBlock => {
                self.emit_code();
                self.in_code = false;
            }
            TagEnd::List(_) => {
                self.lists.pop();
            }
            TagEnd::Item => {
                if !self.cur_cells.is_empty() || !self.block_lines.is_empty() {
                    self.emit_block();
                }
                if let Some(prev) = self.indent_stack.pop() {
                    self.indent = prev;
                }
                self.pending_first = None;
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                if let Some(url) = self.link_url.take() {
                    if !url.is_empty() {
                        self.push_text(
                            &format!(" ({url})"),
                            Style {
                                color: Some(FADE),
                                ..Style::default()
                            },
                        );
                    }
                }
                self.link = self.link.saturating_sub(1);
            }
            _ => {}
        }
    }

    fn start_item(&mut self) {
        let marker = match self.lists.last_mut() {
            Some(Some(n)) => {
                let m = format!("{n}. ");
                *n += 1;
                m
            }
            _ => "• ".to_string(),
        };
        self.pending_first = Some(format!("{}{marker}", self.indent));
        self.indent_stack.push(self.indent.clone());
        self.indent = format!("{}{}", self.indent, " ".repeat(str_width(&marker)));
    }

    /// Current absolute inline style from the nesting counters.
    fn cur_style(&self) -> Style {
        Style {
            bold: self.bold > 0 || self.heading > 0,
            italic: self.italic > 0,
            underline: self.link > 0,
            strike: self.strike > 0,
            code: false,
            color: if self.heading > 0 { Some(ORANGE) } else { None },
        }
    }

    fn push_text(&mut self, s: &str, style: Style) {
        for ch in s.chars() {
            self.cur_cells.push(Cell { ch, style });
        }
    }

    /// Emit any buffered inline content as a block. Used before a nested block
    /// element opens inside a list item so the item's own text is flushed first.
    fn flush_inline(&mut self) {
        if !self.cur_cells.is_empty() || !self.block_lines.is_empty() {
            self.emit_block();
        }
    }

    /// Insert a blank separator line before a top-level block (but never
    /// between list items or quoted paragraphs, and never at the very start).
    fn block_break_if_top(&mut self) {
        if self.quote_depth == 0
            && self.lists.is_empty()
            && !self.out.is_empty()
            && !self.out.ends_with("\n\n")
        {
            self.out.push('\n');
        }
    }

    fn quote_width(&self) -> usize {
        // "│ " is two display columns.
        2 * self.quote_depth
    }

    fn render_quote(&self) -> String {
        if self.quote_depth == 0 {
            String::new()
        } else {
            format!("{}{}{RESET}", sgr_fg(FADE), "│ ".repeat(self.quote_depth))
        }
    }

    /// Flush the current block (paragraph / heading / list-item text) as wrapped,
    /// prefixed, styled scrolled lines.
    fn emit_block(&mut self) {
        if !self.cur_cells.is_empty() {
            self.block_lines.push(std::mem::take(&mut self.cur_cells));
        }
        if self.block_lines.is_empty() {
            return;
        }
        self.block_break_if_top();

        let budget = self
            .cols
            .saturating_sub(self.quote_width() + str_width(&self.indent))
            .max(1);
        let bars = self.render_quote();
        let lines = std::mem::take(&mut self.block_lines);
        let mut first = true;
        for logical in &lines {
            for phys in wrap_cells(logical, budget) {
                let body_prefix = if first && self.pending_first.is_some() {
                    self.pending_first.take().unwrap()
                } else {
                    self.indent.clone()
                };
                self.out.push_str(&bars);
                self.out.push_str(&body_prefix);
                self.out.push_str(&render_cells(&phys));
                self.out.push('\n');
                first = false;
            }
        }
    }

    /// Flush an accumulated code block: dim, two-space inset, never reflowed.
    fn emit_code(&mut self) {
        self.block_break_if_top();
        let bars = self.render_quote();
        let prefix = format!("{}  ", self.indent);
        let body = self.code_buf.strip_suffix('\n').unwrap_or(&self.code_buf);
        for line in body.split('\n') {
            self.out.push_str(&bars);
            self.out.push_str(&prefix);
            self.out.push_str(&sgr_fg(FADE));
            self.out.push_str(line);
            self.out.push_str(RESET);
            self.out.push('\n');
        }
        self.code_buf.clear();
    }

    /// Flush a thematic break as a dim full-width rule.
    fn emit_rule(&mut self) {
        self.block_break_if_top();
        let width = self
            .cols
            .saturating_sub(self.quote_width() + str_width(&self.indent))
            .max(1);
        self.out.push_str(&self.render_quote());
        self.out.push_str(&self.indent);
        self.out.push_str(&sgr_fg(FADE));
        self.out.push_str(&"─".repeat(width));
        self.out.push_str(RESET);
        self.out.push('\n');
    }
}
