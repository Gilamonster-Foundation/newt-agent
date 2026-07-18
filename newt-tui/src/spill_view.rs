//! Pure state model for the live tool-output spill viewport.
//!
//! Terminal I/O deliberately lives elsewhere. This module accepts arbitrary
//! byte chunks, turns them into safe display lines, and describes the rows a
//! renderer should paint.

use std::collections::VecDeque;

#[cfg(test)]
const DEFAULT_VISIBLE_ROWS: usize = 3;
#[cfg(test)]
const DEFAULT_HISTORY_LINES: usize = 4_096;
#[cfg(test)]
const DEFAULT_LINE_CHARS: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gutter {
    Expand,
    Collapse,
    HiddenAbove(usize),
    HiddenBelow(usize),
    Track,
    Thumb,
}

impl Gutter {
    fn glyph(self) -> char {
        match self {
            Self::Expand => '⧉',
            Self::Collapse => '▣',
            Self::HiddenAbove(_) => '▲',
            Self::HiddenBelow(_) => '▼',
            Self::Track => '▒',
            Self::Thumb => '▓',
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RenderedRow {
    pub(crate) gutter: Gutter,
    pub(crate) text: String,
    pub(crate) line: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SpillFrame {
    pub(crate) top: RenderedRow,
    pub(crate) content: Vec<RenderedRow>,
    pub(crate) bottom: RenderedRow,
    pub(crate) dropped_lines: usize,
}

impl SpillFrame {
    #[cfg(test)]
    pub(crate) fn lines(&self) -> Vec<String> {
        std::iter::once(&self.top)
            .chain(&self.content)
            .chain(std::iter::once(&self.bottom))
            .map(|row| row.line.clone())
            .collect()
    }
}

#[derive(Clone, Debug)]
struct StoredLine {
    text: String,
    truncated: bool,
}

impl StoredLine {
    fn display_text(&self) -> String {
        if self.truncated {
            format!("{}…", self.text)
        } else {
            self.text.clone()
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum EscapeState {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    OscEscape,
    String,
    StringEscape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpillStream {
    Stdout,
    Stderr,
}

impl SpillStream {
    fn index(self) -> usize {
        match self {
            Self::Stdout => 0,
            Self::Stderr => 1,
        }
    }
}

enum DecodedEvent {
    Char(char),
    Newline,
}

#[derive(Default)]
struct StreamDecoder {
    utf8_pending: Vec<u8>,
    escape_state: EscapeState,
    skip_lf: bool,
}

impl StreamDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<DecodedEvent> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            self.consume_byte(byte, &mut events);
        }
        events
    }

    fn finish(&mut self) -> Vec<DecodedEvent> {
        self.escape_state = EscapeState::Ground;
        let mut events = Vec::new();
        self.decode_pending(true, &mut events);
        events
    }

    fn consume_byte(&mut self, byte: u8, events: &mut Vec<DecodedEvent>) {
        match self.escape_state {
            EscapeState::Ground => self.consume_ground_byte(byte, events),
            EscapeState::Escape => {
                self.escape_state = match byte {
                    b'[' => EscapeState::Csi,
                    b']' => EscapeState::Osc,
                    b'P' | b'X' | b'^' | b'_' => EscapeState::String,
                    0x20..=0x2f => EscapeState::EscapeIntermediate,
                    0x1b => EscapeState::Escape,
                    _ => EscapeState::Ground,
                };
            }
            EscapeState::EscapeIntermediate => {
                self.escape_state = match byte {
                    0x30..=0x7e => EscapeState::Ground,
                    0x1b => EscapeState::Escape,
                    _ => EscapeState::EscapeIntermediate,
                };
            }
            EscapeState::Csi => {
                if byte == 0x1b {
                    self.escape_state = EscapeState::Escape;
                } else if (0x40..=0x7e).contains(&byte) {
                    self.escape_state = EscapeState::Ground;
                }
            }
            EscapeState::Osc => {
                self.escape_state = match byte {
                    0x07 => EscapeState::Ground,
                    0x1b => EscapeState::OscEscape,
                    _ => EscapeState::Osc,
                };
            }
            EscapeState::OscEscape => {
                self.escape_state = match byte {
                    b'\\' => EscapeState::Ground,
                    0x1b => EscapeState::OscEscape,
                    _ => EscapeState::Osc,
                };
            }
            EscapeState::String => {
                if byte == 0x1b {
                    self.escape_state = EscapeState::StringEscape;
                }
            }
            EscapeState::StringEscape => {
                self.escape_state = match byte {
                    b'\\' => EscapeState::Ground,
                    0x1b => EscapeState::StringEscape,
                    _ => EscapeState::String,
                };
            }
        }
    }

    fn consume_ground_byte(&mut self, byte: u8, events: &mut Vec<DecodedEvent>) {
        match byte {
            0x1b => {
                self.decode_pending(true, events);
                self.escape_state = EscapeState::Escape;
            }
            b'\n' => {
                self.decode_pending(true, events);
                events.push(DecodedEvent::Newline);
            }
            b'\r' => {
                self.decode_pending(true, events);
                events.push(DecodedEvent::Newline);
                self.skip_lf = true;
            }
            b'\t' => {
                self.decode_pending(true, events);
                events.extend((0..4).map(|_| DecodedEvent::Char(' ')));
            }
            0x00..=0x1f | 0x7f => self.decode_pending(true, events),
            _ => {
                self.utf8_pending.push(byte);
                self.decode_pending(false, events);
            }
        }
    }

    fn decode_pending(&mut self, finalize: bool, events: &mut Vec<DecodedEvent>) {
        loop {
            match std::str::from_utf8(&self.utf8_pending) {
                Ok(valid) => {
                    events.extend(valid.chars().map(DecodedEvent::Char));
                    self.utf8_pending.clear();
                    break;
                }
                Err(error) if error.valid_up_to() > 0 => {
                    let valid = self.utf8_pending[..error.valid_up_to()].to_vec();
                    self.utf8_pending.drain(..error.valid_up_to());
                    let valid = std::str::from_utf8(&valid).expect("validated UTF-8 prefix");
                    events.extend(valid.chars().map(DecodedEvent::Char));
                }
                Err(error) => match error.error_len() {
                    Some(len) => {
                        self.utf8_pending.drain(..len);
                        events.push(DecodedEvent::Char('\u{fffd}'));
                    }
                    None if finalize => {
                        self.utf8_pending.clear();
                        events.push(DecodedEvent::Char('\u{fffd}'));
                        break;
                    }
                    None => break,
                },
            }
        }
    }
}

/// Incremental, renderer-neutral state for one active tool's output.
pub(crate) struct SpillView {
    width: usize,
    visible_rows: usize,
    collapsed_rows: usize,
    max_visible_rows: usize,
    expanded: bool,
    history_limit: usize,
    line_char_limit: usize,
    lines: VecDeque<StoredLine>,
    current: String,
    current_chars: usize,
    current_truncated: bool,
    decoders: [StreamDecoder; 2],
    dropped_lines: usize,
    view_start: usize,
    follow_tail: bool,
    finished: bool,
}

impl SpillView {
    #[cfg(test)]
    pub(crate) fn new(width: usize) -> Self {
        Self::with_limits(
            width,
            DEFAULT_VISIBLE_ROWS,
            DEFAULT_HISTORY_LINES,
            DEFAULT_LINE_CHARS,
        )
    }

    pub(crate) fn with_limits(
        width: usize,
        visible_rows: usize,
        history_lines: usize,
        line_chars: usize,
    ) -> Self {
        let history_limit = history_lines.max(1);
        let visible_rows = visible_rows.max(1).min(history_limit);
        Self {
            width,
            visible_rows,
            collapsed_rows: visible_rows,
            max_visible_rows: visible_rows,
            expanded: false,
            history_limit,
            line_char_limit: line_chars.max(1),
            lines: VecDeque::new(),
            current: String::new(),
            current_chars: 0,
            current_truncated: false,
            decoders: std::array::from_fn(|_| StreamDecoder::default()),
            dropped_lines: 0,
            view_start: 0,
            follow_tail: true,
            finished: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn push_bytes(&mut self, bytes: &[u8]) {
        self.push_stream_bytes(SpillStream::Stdout, bytes);
    }

    pub(crate) fn push_stream_bytes(&mut self, stream: SpillStream, bytes: &[u8]) {
        if self.finished {
            return;
        }
        let events = self.decoders[stream.index()].push(bytes);
        self.apply_events(events);
        self.refresh_visible_rows();
    }

    pub(crate) fn resize(&mut self, width: usize, collapsed_rows: usize, max_visible_rows: usize) {
        self.width = width;
        self.max_visible_rows = max_visible_rows.max(1).min(self.history_limit);
        self.collapsed_rows = collapsed_rows.max(1).min(self.max_visible_rows);
        self.refresh_visible_rows();
    }

    #[cfg(any(unix, test))]
    pub(crate) fn toggle_expanded(&mut self) {
        self.expanded = !self.expanded;
        self.refresh_visible_rows();
    }

    pub(crate) fn visible_rows(&self) -> usize {
        self.visible_rows
    }

    fn refresh_visible_rows(&mut self) {
        self.visible_rows = if self.expanded {
            self.retained_line_count().max(1).min(self.max_visible_rows)
        } else {
            self.collapsed_rows
        };
        if !self.follow_tail {
            self.view_start = self.effective_start();
        }
    }

    #[cfg(any(unix, test))]
    pub(crate) fn scroll_up(&mut self) {
        let retained_start = self.dropped_lines;
        let start = self.effective_start();
        if start > retained_start {
            self.follow_tail = false;
            self.view_start = start - 1;
        }
    }

    #[cfg(any(unix, test))]
    pub(crate) fn scroll_down(&mut self) {
        if self.follow_tail {
            return;
        }
        let max_start = self.max_start();
        if self.view_start >= max_start.saturating_sub(1) {
            self.view_start = max_start;
            self.follow_tail = true;
        } else {
            self.view_start += 1;
        }
    }

    pub(crate) fn finish(&mut self) {
        if self.finished {
            return;
        }
        let events = self
            .decoders
            .iter_mut()
            .flat_map(StreamDecoder::finish)
            .collect();
        self.apply_events(events);
        if !self.current.is_empty() || self.current_truncated {
            self.commit_line();
        }
        self.refresh_visible_rows();
        self.finished = true;
    }

    #[cfg(test)]
    pub(crate) fn is_finished(&self) -> bool {
        self.finished
    }

    #[cfg(test)]
    pub(crate) fn is_following_tail(&self) -> bool {
        self.follow_tail
    }

    #[cfg(test)]
    pub(crate) fn line_count(&self) -> usize {
        self.retained_line_count()
    }

    #[cfg(test)]
    pub(crate) fn dropped_lines(&self) -> usize {
        self.dropped_lines
    }

    pub(crate) fn frame(&self) -> SpillFrame {
        let retained_start = self.dropped_lines;
        let retained_count = self.retained_line_count();
        let end = retained_start + retained_count;
        let start = self.effective_start();
        let shown = end.saturating_sub(start).min(self.visible_rows);
        let hidden_above = start.saturating_sub(retained_start);
        let hidden_below = end.saturating_sub(start + shown);

        let boundary = if self.expanded {
            Gutter::Collapse
        } else {
            Gutter::Expand
        };
        let top_gutter = if hidden_above == 0 {
            boundary
        } else {
            Gutter::HiddenAbove(hidden_above)
        };
        let bottom_gutter = if hidden_below == 0 {
            boundary
        } else {
            Gutter::HiddenBelow(hidden_below)
        };

        let thumb = self.thumb_row(start, shown);
        let content = (0..shown)
            .map(|row| {
                let line = self.line_at(start - retained_start + row);
                let gutter = if Some(row) == thumb {
                    Gutter::Thumb
                } else {
                    Gutter::Track
                };
                rendered_row(gutter, &line.display_text(), self.width)
            })
            .collect();

        SpillFrame {
            top: rendered_row(top_gutter, &boundary_text(top_gutter), self.width),
            content,
            bottom: rendered_row(bottom_gutter, &boundary_text(bottom_gutter), self.width),
            dropped_lines: self.dropped_lines,
        }
    }

    fn apply_events(&mut self, events: Vec<DecodedEvent>) {
        for event in events {
            match event {
                DecodedEvent::Char(ch) => self.push_char(ch),
                DecodedEvent::Newline => self.commit_line(),
            }
        }
    }

    fn push_char(&mut self, ch: char) {
        if is_unsafe_display_char(ch) {
            return;
        }
        if self.current_chars == 0 && !self.current_truncated {
            self.drop_oldest_if_full();
        }
        if self.current_chars < self.line_char_limit {
            self.current.push(ch);
            self.current_chars += 1;
        } else {
            self.current_truncated = true;
        }
    }

    fn commit_line(&mut self) {
        if self.current.is_empty() && !self.current_truncated {
            self.drop_oldest_if_full();
        }
        self.lines.push_back(StoredLine {
            text: std::mem::take(&mut self.current),
            truncated: std::mem::take(&mut self.current_truncated),
        });
        self.current_chars = 0;
        debug_assert!(self.lines.len() <= self.history_limit);
    }

    fn drop_oldest_if_full(&mut self) {
        if self.lines.len() >= self.history_limit {
            self.lines.pop_front();
            self.dropped_lines += 1;
        }
        if !self.follow_tail && self.view_start < self.dropped_lines {
            self.view_start = self.dropped_lines;
        }
    }

    fn retained_line_count(&self) -> usize {
        self.lines.len() + usize::from(!self.current.is_empty() || self.current_truncated)
    }

    fn line_at(&self, relative: usize) -> StoredLine {
        if let Some(line) = self.lines.get(relative) {
            line.clone()
        } else {
            StoredLine {
                text: self.current.clone(),
                truncated: self.current_truncated,
            }
        }
    }

    fn max_start(&self) -> usize {
        let end = self.dropped_lines + self.retained_line_count();
        end.saturating_sub(self.visible_rows)
            .max(self.dropped_lines)
    }

    fn effective_start(&self) -> usize {
        let max_start = self.max_start();
        if self.follow_tail {
            max_start
        } else {
            self.view_start.clamp(self.dropped_lines, max_start)
        }
    }

    fn thumb_row(&self, start: usize, shown: usize) -> Option<usize> {
        if shown == 0 {
            return None;
        }
        let max_offset = self.max_start().saturating_sub(self.dropped_lines);
        if max_offset == 0 {
            return None;
        }
        let offset = start.saturating_sub(self.dropped_lines).min(max_offset);
        Some((offset * (shown - 1) + max_offset / 2) / max_offset)
    }
}

fn boundary_text(gutter: Gutter) -> String {
    match gutter {
        Gutter::HiddenAbove(lines) => hidden_text(lines, "above"),
        Gutter::HiddenBelow(lines) => hidden_text(lines, "below"),
        // #1263: the LIVE viewport advertises its keys at the point of use —
        // the bare ⧉/▣ glyph told nobody the frame was interactive (its keys
        // were hinted only in /help), so the inert completed excerpt could
        // masquerade as this scroller. Honest labeling, still a plain scroller.
        Gutter::Expand => "Space expands · ↑↓ scroll".to_string(),
        Gutter::Collapse => "Space collapses · ↑↓ scroll".to_string(),
        _ => String::new(),
    }
}

fn hidden_text(lines: usize, direction: &str) -> String {
    let noun = if lines == 1 { "line" } else { "lines" };
    format!("{lines} more {noun} {direction}")
}

fn rendered_row(gutter: Gutter, text: &str, width: usize) -> RenderedRow {
    // Leave the terminal's final column untouched. Printing into that cell can
    // set delayed-wrap state, which would move a cursor-addressed repaint down
    // an extra row on the next write.
    let width = width.saturating_sub(1);
    let prefix = gutter.glyph().to_string();
    // #1263: Expand/Collapse rows carry the key legend now (from
    // `boundary_text`); every boundary row width-clips the same way.
    let clipped = clip_to_width(text, width.saturating_sub(2));
    let line = match width {
        0 => String::new(),
        1 => prefix,
        _ if clipped.is_empty() => format!("{prefix} "),
        _ => format!("{prefix} {clipped}"),
    };
    RenderedRow {
        gutter,
        text: clipped,
        line,
    }
}

fn clip_to_width(text: &str, width: usize) -> String {
    let mut used = 0;
    text.chars()
        .take_while(|&ch| {
            let next = used + char_width(ch);
            if next > width {
                false
            } else {
                used = next;
                true
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

fn char_width(ch: char) -> usize {
    if is_combining(ch) {
        0
    } else if ch.is_ascii() || matches!(ch, '…' | '▲' | '▼' | '▒' | '▓' | '⧉' | '▣' | '\u{fffd}')
    {
        1
    } else {
        // Conservatively budget two cells for non-ASCII. Over-counting narrow
        // glyphs wastes a column; under-counting a wide glyph can autowrap and
        // corrupt the active redraw region.
        2
    }
}

fn is_unsafe_display_char(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
        )
}

fn is_combining(ch: char) -> bool {
    matches!(
        ch,
        '\u{0300}'..='\u{036f}'
            | '\u{1ab0}'..='\u{1aff}'
            | '\u{1dc0}'..='\u{1dff}'
            | '\u{20d0}'..='\u{20ff}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{fe20}'..='\u{fe2f}'
            | '\u{e0100}'..='\u{e01ef}'
    )
}

#[cfg(test)]
mod tests {
    use super::{display_width, Gutter, SpillStream, SpillView};

    fn feed_lines(view: &mut SpillView, lines: &[&str]) {
        for line in lines {
            view.push_bytes(line.as_bytes());
            view.push_bytes(b"\n");
        }
    }

    #[test]
    fn defaults_to_three_rows_and_follows_the_tail() {
        let mut view = SpillView::new(80);
        feed_lines(&mut view, &["l1", "l2", "l3", "l4", "l5"]);

        let frame = view.frame();
        assert!(view.is_following_tail());
        assert_eq!(frame.content.len(), 3);
        assert_eq!(
            frame.lines(),
            [
                "▲ 2 more lines above",
                "▒ l3",
                "▒ l4",
                "▓ l5",
                "⧉ Space expands · ↑↓ scroll",
            ]
        );
    }

    #[test]
    fn up_detaches_and_down_at_the_bottom_reattaches() {
        let mut view = SpillView::new(80);
        feed_lines(&mut view, &["l1", "l2", "l3", "l4", "l5"]);

        view.scroll_up();
        assert!(!view.is_following_tail());
        let frame = view.frame();
        assert_eq!(
            frame.lines(),
            [
                "▲ 1 more line above",
                "▒ l2",
                "▓ l3",
                "▒ l4",
                "▼ 1 more line below",
            ]
        );

        view.scroll_up();
        let frame = view.frame();
        assert_eq!(
            frame.lines(),
            [
                "⧉ Space expands · ↑↓ scroll",
                "▓ l1",
                "▒ l2",
                "▒ l3",
                "▼ 2 more lines below",
            ]
        );

        view.scroll_down();
        assert!(!view.is_following_tail());
        view.scroll_down();
        assert!(view.is_following_tail());
        assert_eq!(view.frame().content[2].gutter, Gutter::Thumb);
    }

    #[test]
    fn detached_view_stays_put_while_new_output_arrives() {
        let mut view = SpillView::new(80);
        feed_lines(&mut view, &["l1", "l2", "l3", "l4"]);
        view.scroll_up();
        let before = view.frame().content;

        feed_lines(&mut view, &["l5", "l6"]);

        assert_eq!(view.frame().content, before);
        assert!(!view.is_following_tail());
        assert_eq!(view.frame().bottom.gutter, Gutter::HiddenBelow(3));
    }

    #[test]
    fn split_utf8_and_partial_final_line_are_preserved() {
        let mut view = SpillView::new(80);
        view.push_bytes(b"caf");
        view.push_bytes(&[0xc3]);
        view.push_bytes(&[0xa9]);
        assert_eq!(view.frame().content[0].text, "café");

        view.finish();
        assert!(view.is_finished());
        assert_eq!(view.line_count(), 1);
        assert_eq!(view.frame().content[0].text, "café");

        // Finalization is idempotent and late bytes cannot mutate the snapshot.
        view.finish();
        view.push_bytes(b" ignored");
        assert_eq!(view.line_count(), 1);
        assert_eq!(view.frame().content[0].text, "café");
    }

    #[test]
    fn terminal_controls_are_removed_across_chunk_boundaries() {
        let mut view = SpillView::new(80);
        view.push_bytes(b"safe\x1b[3");
        view.push_bytes(b"1mred\x1b[0m \x1b(0\x1b]0;owned");
        view.push_bytes(b"\x07done\nbad\rnext\tcol\x08!\n");

        assert_eq!(
            view.frame()
                .content
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            ["safered done", "bad", "next    col!"]
        );
    }

    #[test]
    fn stdout_decoder_state_cannot_consume_or_corrupt_stderr() {
        let mut view = SpillView::new(80);
        view.push_stream_bytes(SpillStream::Stdout, b"\x1b]0;stdout-title");
        view.push_stream_bytes(SpillStream::Stderr, b"stderr stays visible\n");
        view.push_stream_bytes(SpillStream::Stdout, b"\x07stdout visible\n");

        assert_eq!(
            view.frame()
                .content
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            ["stderr stays visible", "stdout visible"]
        );

        let mut split_utf8 = SpillView::new(80);
        split_utf8.push_stream_bytes(SpillStream::Stdout, &[0xc3]);
        split_utf8.push_stream_bytes(SpillStream::Stderr, b"err\n");
        split_utf8.push_stream_bytes(SpillStream::Stdout, &[0xa9, b'\n']);
        assert_eq!(
            split_utf8
                .frame()
                .content
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            ["err", "é"]
        );
    }

    #[test]
    fn every_rendered_row_is_clipped_before_it_can_autowrap() {
        let mut view = SpillView::new(8);
        feed_lines(&mut view, &["abcdefghij", "界界界界", "tail"]);

        let frame = view.frame();
        assert_eq!(frame.content[0].line, "▒ abcde");
        assert_eq!(frame.content[1].line, "▒ 界界");
        for line in frame.lines() {
            assert!(display_width(&line) < 8, "row escaped width: {line:?}");
        }
    }

    #[test]
    fn resize_reclips_rows_and_preserves_detached_position() {
        let mut view = SpillView::with_limits(80, 3, 10, 80);
        feed_lines(&mut view, &["abcdefghij", "second", "third", "fourth"]);
        view.scroll_up();

        view.resize(8, 2, 2);

        assert!(!view.is_following_tail());
        assert_eq!(view.frame().content.len(), 2);
        assert_eq!(view.frame().content[0].text, "abcde");
        for line in view.frame().lines() {
            assert!(display_width(&line) < 8, "row escaped width: {line:?}");
        }
    }

    #[test]
    fn boundary_control_toggles_between_collapsed_and_expanded_states() {
        let mut view = SpillView::with_limits(80, 3, 10, 80);
        view.resize(80, 3, 10);
        feed_lines(
            &mut view,
            &["first", "second", "third", "fourth", "fifth", "sixth"],
        );

        assert_eq!(
            view.frame().lines(),
            [
                "▲ 3 more lines above",
                "▒ fourth",
                "▒ fifth",
                "▓ sixth",
                "⧉ Space expands · ↑↓ scroll"
            ]
        );

        view.toggle_expanded();
        let expanded = view.frame();
        assert_eq!(expanded.top.line, "▣ Space collapses · ↑↓ scroll");
        assert_eq!(expanded.bottom.line, "▣ Space collapses · ↑↓ scroll");
        assert_eq!(expanded.content.len(), 6);
        assert!(expanded
            .content
            .iter()
            .all(|row| row.gutter == Gutter::Track));

        view.toggle_expanded();
        assert_eq!(view.frame().content.len(), 3);
        assert_eq!(view.frame().bottom.line, "⧉ Space expands · ↑↓ scroll");
    }

    /// #1263: the LIVE boundary rows advertise their keys at the point of use
    /// — the interactivity legend, not a bare glyph nobody can decode.
    #[test]
    fn live_boundary_row_advertises_expand_key() {
        let mut view = SpillView::new(80);
        feed_lines(&mut view, &["l1", "l2", "l3", "l4", "l5"]);
        let frame = view.frame();
        assert!(
            frame.bottom.line.contains("Space expands"),
            "collapsed boundary must carry the key legend: {:?}",
            frame.bottom.line
        );
        view.toggle_expanded();
        assert!(
            view.frame().bottom.line.contains("Space collapses"),
            "expanded boundary must carry the collapse legend"
        );
    }

    /// #1263: the interactivity fingerprint, pinned — a live frame's boundary
    /// leads with ⧉/▣ (plus its legend); the completed excerpt's last row is
    /// the inert `…` (asserted in display.rs). The distinction cannot regress
    /// silently.
    #[test]
    fn live_frame_boundary_leads_with_the_interactive_glyph() {
        let mut view = SpillView::new(80);
        feed_lines(&mut view, &["l1", "l2", "l3", "l4", "l5"]);
        assert!(view.frame().bottom.line.starts_with('⧉'));
        view.toggle_expanded();
        assert!(view.frame().bottom.line.starts_with('▣'));
    }

    /// #1263: the legend width-clips on narrow terminals like any boundary
    /// text — never escaping the row or panicking.
    #[test]
    fn legend_clips_to_narrow_widths() {
        for width in [0usize, 1, 2, 8, 12] {
            let mut view = SpillView::new(width);
            feed_lines(&mut view, &["l1", "l2", "l3", "l4", "l5"]);
            let line = view.frame().bottom.line.clone();
            assert!(
                display_width(&line) < width.max(1),
                "width {width}: row escaped its budget: {line:?}"
            );
        }
    }

    #[test]
    fn expanded_view_has_no_thumb_when_all_retained_lines_fit_after_history_drops() {
        let mut view = SpillView::with_limits(80, 2, 3, 80);
        view.resize(80, 2, 3);
        feed_lines(&mut view, &["first", "second", "third", "fourth", "fifth"]);
        assert_eq!(view.dropped_lines(), 2);

        view.toggle_expanded();

        assert_eq!(view.frame().content.len(), 3);
        assert!(view
            .frame()
            .content
            .iter()
            .all(|row| row.gutter == Gutter::Track));
    }

    #[test]
    fn history_and_individual_lines_are_bounded() {
        let mut view = SpillView::with_limits(80, 3, 4, 5);
        feed_lines(&mut view, &["first-too-long", "l2", "l3"]);
        assert_eq!(view.frame().content[0].text, "first…");

        feed_lines(&mut view, &["l4", "l5", "l6"]);

        assert_eq!(view.dropped_lines(), 2);
        assert_eq!(view.line_count(), 4);
        assert_eq!(
            view.frame().lines(),
            [
                "▲ 1 more line above",
                "▒ l4",
                "▒ l5",
                "▓ l6",
                "⧉ Space expands · ↑↓ scroll",
            ]
        );

        view.scroll_up();
        assert_eq!(
            view.frame().lines(),
            [
                "⧉ Space expands · ↑↓ scroll",
                "▓ l3",
                "▒ l4",
                "▒ l5",
                "▼ 1 more line below",
            ]
        );

        // A partial live line also consumes one bounded history slot.
        view.push_bytes(b"l7-partial");
        assert_eq!(view.line_count(), 4);
        assert_eq!(view.dropped_lines(), 3);
    }

    #[test]
    fn terminal_capacity_cannot_raise_the_retained_history_limit() {
        let mut view = SpillView::with_limits(80, 10, 3, 80);
        view.resize(80, 10, 10);
        feed_lines(&mut view, &["one", "two", "three", "four", "five"]);

        assert_eq!(view.visible_rows(), 3);
        assert_eq!(view.line_count(), 3);
        assert_eq!(view.dropped_lines(), 2);
    }

    #[test]
    fn invalid_utf8_is_rendered_as_safe_text() {
        let mut view = SpillView::new(80);
        view.push_bytes(b"a\xffb\n");
        assert_eq!(view.frame().content[0].text, "a�b");
    }
}
