//! The transcript stream: bytes off the session's pty, turned into lines.
//!
//! Everything the session writes to fd 1/2 arrives here as a byte stream —
//! transcript lines, the spinner's `\r ESC[K …` frames, a permission
//! question, a tracing `WARN`. This module is the ONE place that decides what
//! is a finished line (goes into scrollback above the cockpit), what is the
//! in-progress row (shown as the cockpit's status row), and what must be
//! forwarded to the real terminal untouched (an allowlisted mouse-tracking
//! mode — meaningful to the tty; see [`private_mode_forwarded`]). Bracketed
//! paste, the alternate screen, cursor show/hide and autowrap belong to the
//! presenter, so a child or tool cannot toggle them through this gate.
//!
//! It is a scanner over the sequences *newt itself emits* (crossterm output),
//! not a VT emulator. Cursor motion is dropped on purpose: under the cockpit
//! the two cursor-relative renderers are not constructed, so anything moving
//! the cursor is a stray, and passing it through would let it move the
//! terminal's real cursor into the editor. Unknown sequences are dropped, not
//! forwarded — forwarding is the dangerous default here.

use newt_core::tty::str_width;

/// A control sequence the scanner recognised, in the terms the stream needs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A printable UTF-8 chunk (no escapes, no `\n`, no `\r`).
    Text(Vec<u8>),
    Newline,
    CarriageReturn,
    /// `ESC[…m` — kept inline in the row bytes; the terminal styles it.
    Sgr(Vec<u8>),
    /// `ESC[K` (0) · `ESC[1K` · `ESC[2K`.
    EraseInLine(u8),
    /// `ESC[nG` — 1-based column.
    MoveToColumn(usize),
    /// `ESC[?…h` / `ESC[?…l` — an allowlisted (mouse-tracking) DEC private mode,
    /// forwarded verbatim. Presenter-owned modes never become this token.
    PrivateMode(Vec<u8>),
    /// Anything else the scanner could delimit — dropped.
    Other,
}

/// Cut `bytes` into tokens. Incomplete trailing sequences are returned as the
/// unconsumed tail so a chunk boundary inside `ESC[` never mis-tokenises.
fn scan(bytes: &[u8]) -> (Vec<Token>, Vec<u8>) {
    let mut out = Vec::new();
    let mut text = Vec::new();
    let mut i = 0;
    let flush_text = |text: &mut Vec<u8>, out: &mut Vec<Token>| {
        if !text.is_empty() {
            out.push(Token::Text(std::mem::take(text)));
        }
    };
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                flush_text(&mut text, &mut out);
                out.push(Token::Newline);
                i += 1;
            }
            b'\r' => {
                flush_text(&mut text, &mut out);
                out.push(Token::CarriageReturn);
                i += 1;
            }
            0x1b => {
                flush_text(&mut text, &mut out);
                match scan_escape(&bytes[i..]) {
                    Some((tok, len)) => {
                        out.push(tok);
                        i += len;
                    }
                    // Incomplete: hand the tail back for the next chunk.
                    None => return (out, bytes[i..].to_vec()),
                }
            }
            // Other C0 controls (BEL, BS, TAB…): TAB is text to a terminal
            // (it advances the cursor); the rest are dropped so they cannot
            // ring or backspace into the cockpit.
            b'\t' => {
                text.push(b'\t');
                i += 1;
            }
            0x00..=0x1f | 0x7f => {
                i += 1;
            }
            _ => {
                text.push(bytes[i]);
                i += 1;
            }
        }
    }
    // Hold back an incomplete trailing UTF-8 sequence, exactly as the escape
    // scanner holds back an incomplete `ESC[`: a multibyte char split across a
    // chunk boundary (a spinner glyph, CJK, a combining mark) must not be
    // tokenised as two partial `Text` runs, or its width is mis-measured and it
    // decodes to replacement characters. What remains in `text` is all complete
    // chars; the carried tail is prepended to the next chunk by `feed`.
    let carry = split_incomplete_utf8(&mut text);
    flush_text(&mut text, &mut out);
    (out, carry)
}

/// Split off any trailing bytes of `text` that begin — but do not complete — a
/// UTF-8 character, returning them to be carried into the next chunk. A
/// genuinely invalid sequence in the interior is left in place for lossy
/// decoding; only a truncation at the very end is held back.
fn split_incomplete_utf8(text: &mut Vec<u8>) -> Vec<u8> {
    match std::str::from_utf8(text) {
        Ok(_) => Vec::new(),
        // `error_len() == None` is std's signal for "valid so far, but the
        // input ended mid-character" — precisely the chunk-boundary split. The
        // incomplete bytes start at `valid_up_to`.
        Err(e) if e.error_len().is_none() => text.split_off(e.valid_up_to()),
        // A real encoding error (`Some(len)`) is not a boundary artefact; leave
        // it for `from_utf8_lossy` to replace so we still make progress.
        Err(_) => Vec::new(),
    }
}

/// Delimit one escape sequence starting at `bytes[0] == ESC`. Returns the
/// token and its byte length, or `None` when the sequence is incomplete.
fn scan_escape(bytes: &[u8]) -> Option<(Token, usize)> {
    let &second = bytes.get(1)?;
    match second {
        b'[' => {
            // CSI: params 0x30–0x3F, intermediates 0x20–0x2F, final 0x40–0x7E.
            let mut j = 2;
            while j < bytes.len() && (0x30..=0x3f).contains(&bytes[j]) {
                j += 1;
            }
            while j < bytes.len() && (0x20..=0x2f).contains(&bytes[j]) {
                j += 1;
            }
            let &fin = bytes.get(j)?;
            if !(0x40..=0x7e).contains(&fin) {
                // Malformed; consume the ESC alone so we make progress.
                return Some((Token::Other, 1));
            }
            let seq = &bytes[..=j];
            let params = &bytes[2..j];
            let tok = match fin {
                b'm' => Token::Sgr(seq.to_vec()),
                b'K' => Token::EraseInLine(first_param(params).unwrap_or(0) as u8),
                b'G' => Token::MoveToColumn(first_param(params).unwrap_or(1).max(1)),
                // A DEC private mode. Forward ONLY the allowlisted mouse family
                // to the real terminal; drop everything else so a child or tool
                // printing to the pty cannot switch the alternate screen, hide
                // the cursor, retoggle bracketed paste, or disable autowrap —
                // all state the presenter owns (#3). `params[1..]` skips the `?`.
                b'h' | b'l' if params.first() == Some(&b'?') => {
                    if private_mode_forwarded(&params[1..]) {
                        Token::PrivateMode(seq.to_vec())
                    } else {
                        Token::Other
                    }
                }
                _ => Token::Other,
            };
            Some((tok, j + 1))
        }
        b']' => {
            // OSC: until BEL or ESC \ . Dropped either way (titles etc. are
            // not the cockpit's to relay in v1).
            let mut j = 2;
            while j < bytes.len() {
                if bytes[j] == 0x07 {
                    return Some((Token::Other, j + 1));
                }
                if bytes[j] == 0x1b && bytes.get(j + 1) == Some(&b'\\') {
                    return Some((Token::Other, j + 2));
                }
                j += 1;
            }
            None
        }
        // Two-byte ESC sequences (ESC 7 / ESC 8 / ESC = …): dropped.
        _ => Some((Token::Other, 2)),
    }
}

fn first_param(params: &[u8]) -> Option<usize> {
    let digits: Vec<u8> = params
        .iter()
        .copied()
        .take_while(u8::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        return None;
    }
    std::str::from_utf8(&digits).ok()?.parse().ok()
}

/// DEC private modes the cockpit forwards from the session's pty to the real
/// terminal — the mouse-tracking family and its coordinate encodings only.
/// These change what the terminal DELIVERS to stdin (input side); they touch
/// none of the presenter-owned OUTPUT state. Deliberately excluded, and dropped
/// by [`private_mode_forwarded`]: alternate screen (47 / 1047 / 1048 / 1049),
/// cursor visibility (25), bracketed paste (2004), autowrap (7), and every
/// other unlisted mode — the presenter owns each of those and a child/tool must
/// not be able to move them out from under it.
const FORWARDED_PRIVATE_MODES: &[u16] = &[1000, 1002, 1003, 1005, 1006, 1015, 1016];

/// Whether a DEC private-mode parameter list — the bytes between `?` and the
/// final `h`/`l` — may be forwarded. True only when it is non-empty and EVERY
/// `;`-separated parameter is allowlisted; an empty, mixed, or unparseable set
/// is dropped whole. Default-deny is the safe posture here (the module's own
/// doctrine: "forwarding is the dangerous default").
fn private_mode_forwarded(params: &[u8]) -> bool {
    let mut any = false;
    for part in params.split(|&b| b == b';') {
        any = true;
        let Ok(text) = std::str::from_utf8(part) else {
            return false;
        };
        let Ok(mode) = text.parse::<u16>() else {
            return false;
        };
        if !FORWARDED_PRIVATE_MODES.contains(&mode) {
            return false;
        }
    }
    any
}

/// One row of styled bytes with no `\n`/`\r` and no cursor motion — safe to
/// write to the terminal at a known position, followed by `\r\n`.
pub(crate) type Row = Vec<u8>;

/// What one chunk of session output produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Drained {
    /// Finished rows, in order, for scrollback.
    pub(crate) lines: Vec<Row>,
    /// Sequences to forward to the real terminal verbatim (DEC private modes).
    pub(crate) passthrough: Vec<u8>,
    /// Whether the in-progress row changed (the status row needs a repaint).
    pub(crate) partial_changed: bool,
}

/// The stream model: complete lines out, an in-progress row kept.
#[derive(Debug, Default)]
pub(crate) struct TranscriptStream {
    /// Bytes held back because they ended mid-escape.
    carry: Vec<u8>,
    /// The in-progress row (no newline yet). The spinner lives here.
    partial: Row,
    /// Where the next print lands in `partial`, in display columns. `\r` and
    /// `ESC[nG` move it; text past the end appends, text before the end
    /// truncates then appends — the terminal would overwrite in place, and for
    /// the sequences newt emits (always erase-then-write) the two agree.
    col: usize,
}

impl TranscriptStream {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The in-progress row as it currently stands.
    pub(crate) fn partial(&self) -> &[u8] {
        &self.partial
    }

    /// Feed a chunk. Returns the finished lines and what else changed.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Drained {
        let mut input = std::mem::take(&mut self.carry);
        input.extend_from_slice(chunk);
        let (tokens, carry) = scan(&input);
        self.carry = carry;
        let mut out = Drained::default();
        for tok in tokens {
            match tok {
                Token::Text(t) => {
                    self.overwrite_at_col();
                    self.partial.extend_from_slice(&t);
                    self.col += str_width(&String::from_utf8_lossy(&t));
                    out.partial_changed = true;
                }
                Token::Sgr(seq) => {
                    // Styling never moves the column.
                    self.overwrite_at_col();
                    self.partial.extend_from_slice(&seq);
                    out.partial_changed = true;
                }
                Token::Newline => {
                    out.lines.push(std::mem::take(&mut self.partial));
                    self.col = 0;
                    out.partial_changed = true;
                }
                Token::CarriageReturn => {
                    self.col = 0;
                }
                Token::MoveToColumn(n) => {
                    self.col = n - 1;
                }
                Token::EraseInLine(mode) => {
                    match mode {
                        // From the cursor to the end.
                        0 => self.truncate_to_col(),
                        // Whole line, either way (1 = to the start, which for a
                        // status row we treat the same as all).
                        _ => {
                            self.partial.clear();
                            self.col = 0;
                        }
                    }
                    out.partial_changed = true;
                }
                Token::PrivateMode(seq) => out.passthrough.extend_from_slice(&seq),
                Token::Other => {}
            }
        }
        out
    }

    /// Prepare `partial` for a print at `self.col`: if the column is inside
    /// the existing row, drop what lies at and after it.
    fn overwrite_at_col(&mut self) {
        if self.col < visible_width(&self.partial) {
            self.truncate_to_col();
        }
    }

    fn truncate_to_col(&mut self) {
        let col = self.col;
        self.partial = clip_to_width(&self.partial, col);
    }
}

/// Display width of a row, ignoring its embedded SGR sequences.
pub(crate) fn visible_width(row: &[u8]) -> usize {
    let (tokens, _) = scan(row);
    tokens
        .iter()
        .map(|t| match t {
            Token::Text(t) => str_width(&String::from_utf8_lossy(t)),
            _ => 0,
        })
        .sum()
}

/// The longest prefix of `row` whose visible width is ≤ `cols`, keeping every
/// SGR sequence that precedes the cut so styling stays balanced.
pub(crate) fn clip_to_width(row: &[u8], cols: usize) -> Row {
    let (tokens, _) = scan(row);
    let mut out = Vec::new();
    let mut used = 0usize;
    for tok in tokens {
        match tok {
            Token::Text(t) => {
                if used >= cols {
                    continue;
                }
                let s = String::from_utf8_lossy(&t);
                let mut taken = String::new();
                for ch in s.chars() {
                    let w = newt_core::tty::ch_width(ch);
                    if used + w > cols {
                        break;
                    }
                    used += w;
                    taken.push(ch);
                }
                out.extend_from_slice(taken.as_bytes());
            }
            Token::Sgr(seq) => out.extend_from_slice(&seq),
            _ => {}
        }
    }
    out
}

/// Split one row into physical rows of at most `cols` visible columns, so
/// every row the cockpit writes occupies exactly one terminal row (autowrap
/// is off while the cockpit owns the terminal). Styling carries across the
/// split because each SGR sequence is kept where it fell.
pub(crate) fn wrap_row(row: &[u8], cols: usize) -> Vec<Row> {
    let cols = cols.max(1);
    let (tokens, _) = scan(row);
    let mut rows: Vec<Row> = Vec::new();
    let mut cur: Row = Vec::new();
    let mut used = 0usize;
    for tok in tokens {
        match tok {
            Token::Text(t) => {
                for ch in String::from_utf8_lossy(&t).chars() {
                    let w = newt_core::tty::ch_width(ch);
                    if used + w > cols && used > 0 {
                        rows.push(std::mem::take(&mut cur));
                        used = 0;
                    }
                    let mut buf = [0u8; 4];
                    cur.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    used += w;
                }
            }
            Token::Sgr(seq) => cur.extend_from_slice(&seq),
            _ => {}
        }
    }
    rows.push(cur);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(s: &mut TranscriptStream, chunks: &[&[u8]]) -> Vec<String> {
        let mut lines = Vec::new();
        for c in chunks {
            lines.extend(
                s.feed(c)
                    .lines
                    .into_iter()
                    .map(|l| String::from_utf8_lossy(&l).into_owned()),
            );
        }
        lines
    }

    #[test]
    fn plain_lines_split_on_newline_and_keep_the_partial() {
        let mut s = TranscriptStream::new();
        assert_eq!(feed_all(&mut s, &[b"one\ntwo\nthr"]), ["one", "two"]);
        assert_eq!(s.partial(), b"thr");
        assert_eq!(feed_all(&mut s, &[b"ee\n"]), ["three"]);
        assert_eq!(s.partial(), b"");
    }

    /// The spinner's frame: `\r ESC[K text`, repeated with no newline. Each
    /// frame REPLACES the status row; nothing is ever emitted as a line.
    #[test]
    fn spinner_frames_replace_the_partial_and_never_become_lines() {
        let mut s = TranscriptStream::new();
        let d = s.feed(b"\r\x1b[K\xe2\xa0\x8b thinking\xe2\x80\xa6 0.1s");
        assert!(d.lines.is_empty());
        assert!(d.partial_changed);
        let d = s.feed(b"\r\x1b[K\xe2\xa0\x99 thinking\xe2\x80\xa6 0.2s");
        assert!(d.lines.is_empty());
        assert_eq!(
            String::from_utf8_lossy(s.partial()),
            "⠙ thinking… 0.2s",
            "the second frame fully replaced the first"
        );
    }

    /// `LineLease::emit_line`: erase the ephemeral row, write a permanent line
    /// with `\n`, and the spinner repaints on the next tick. The permanent
    /// line comes out clean; the stale frame does not leak into it.
    #[test]
    fn erase_then_permanent_line_yields_a_clean_line() {
        let mut s = TranscriptStream::new();
        s.feed(b"\r\x1b[K\xe2\xa0\x8b thinking\xe2\x80\xa6 0.1s");
        let d = s.feed(b"\r\x1b[K  retrying in 1s\n");
        assert_eq!(d.lines, vec![b"  retrying in 1s".to_vec()]);
        assert_eq!(s.partial(), b"");
    }

    /// SGR rides along inside the row bytes — the terminal styles it, and
    /// width is measured on the visible text only.
    #[test]
    fn sgr_is_kept_inline_and_ignored_for_width() {
        let mut s = TranscriptStream::new();
        let d = s.feed(b"\x1b[38;5;208m\xe2\x9a\x99  read_file\x1b[0m: x\n");
        assert_eq!(d.lines.len(), 1);
        assert!(d.lines[0].starts_with(b"\x1b[38;5;208m"));
        assert_eq!(
            visible_width(&d.lines[0]),
            "⚙  read_file: x".chars().count() + 1 - 1
        );
        assert_eq!(visible_width(b"\x1b[1mab\x1b[0m"), 2);
    }

    /// Allowlisted DEC private modes (the mouse family) are for the real
    /// terminal, not for a line: forwarded verbatim, never in a row.
    #[test]
    fn allowlisted_private_modes_pass_through_and_leave_no_trace_in_rows() {
        let mut s = TranscriptStream::new();
        let d = s.feed(b"\x1b[?1000h\x1b[?1006hhello\n\x1b[?1000l");
        assert_eq!(d.passthrough, b"\x1b[?1000h\x1b[?1006h\x1b[?1000l");
        assert_eq!(d.lines, vec![b"hello".to_vec()]);
    }

    /// #3 hardening: presenter-owned modes from child/tool output are DROPPED,
    /// never forwarded — a tool cannot switch the alternate screen, hide the
    /// cursor, retoggle bracketed paste, or disable autowrap under the cockpit.
    #[test]
    fn presenter_owned_private_modes_are_dropped_not_forwarded() {
        let mut s = TranscriptStream::new();
        let d = s.feed(b"\x1b[?1049h\x1b[?25l\x1b[?2004l\x1b[?7lhi\n\x1b[?1049l\x1b[?25h\x1b[?7h");
        assert!(
            d.passthrough.is_empty(),
            "alt-screen/cursor/paste/wrap must never reach the terminal: {:?}",
            d.passthrough
        );
        assert_eq!(d.lines, vec![b"hi".to_vec()], "text between them survives");
    }

    /// The mouse-tracking family — single and `;`-combined, enable and disable —
    /// is the one allowlisted family, so newt's own capture and an interactive
    /// child tool keep working.
    #[test]
    fn mouse_tracking_private_modes_are_forwarded() {
        let mut s = TranscriptStream::new();
        let d = s.feed(b"\x1b[?1000h\x1b[?1002;1003;1006h\x1b[?1000;1006l");
        assert_eq!(
            d.passthrough,
            b"\x1b[?1000h\x1b[?1002;1003;1006h\x1b[?1000;1006l",
        );
    }

    /// Default-deny: a mouse mode paired with a presenter-owned one in one `;`
    /// set is dropped whole, not partially applied.
    #[test]
    fn a_mixed_private_mode_set_is_dropped_whole() {
        let mut s = TranscriptStream::new();
        let d = s.feed(b"\x1b[?1000;25hx\n");
        assert!(
            d.passthrough.is_empty(),
            "1000 is allowlisted but 25 is not — deny the set: {:?}",
            d.passthrough
        );
        assert_eq!(d.lines, vec![b"x".to_vec()]);
    }

    /// Cursor motion is DROPPED, not forwarded — forwarding would move the
    /// terminal's real cursor into the cockpit. Pinned because "pass unknown
    /// through" is the tempting default and the wrong one here.
    #[test]
    fn cursor_motion_is_dropped_never_forwarded() {
        let mut s = TranscriptStream::new();
        let d = s.feed(b"\x1b[3A\x1b[Jgone\n\x1b[2Bx");
        assert!(d.passthrough.is_empty());
        assert_eq!(d.lines, vec![b"gone".to_vec()]);
        assert_eq!(s.partial(), b"x");
    }

    /// The modal prompt's redraw: `ESC[1G ESC[2K prompt value`.
    #[test]
    fn move_to_column_and_erase_whole_line_redraw_the_partial() {
        let mut s = TranscriptStream::new();
        s.feed(b"allow? y");
        s.feed(b"\x1b[1G\x1b[2Kallow? ye");
        assert_eq!(s.partial(), b"allow? ye");
    }

    /// A chunk boundary inside an escape sequence must not mis-tokenise: the
    /// tail is carried into the next feed.
    #[test]
    fn an_escape_split_across_chunks_is_reassembled() {
        let mut s = TranscriptStream::new();
        let d1 = s.feed(b"a\x1b[3");
        assert!(d1.lines.is_empty());
        assert_eq!(
            s.partial(),
            b"a",
            "the half-sequence is held back, not printed"
        );
        let d2 = s.feed(b"8;5;1mb\n");
        assert_eq!(d2.lines, vec![b"a\x1b[38;5;1mb".to_vec()]);
    }

    /// #5: a multibyte glyph split across a chunk boundary is reassembled, not
    /// mangled into replacement characters. The braille spinner frame `⠋` (E2
    /// A0 8B) arrives two bytes then one.
    #[test]
    fn a_spinner_glyph_split_across_chunks_is_reassembled_not_mangled() {
        let mut s = TranscriptStream::new();
        let d1 = s.feed(b"\r\x1b[K\xe2\xa0");
        assert!(d1.lines.is_empty());
        assert_eq!(s.partial(), b"", "the half glyph is held back, not shown");
        s.feed(b"\x8b thinking");
        let partial = String::from_utf8_lossy(s.partial());
        assert_eq!(partial, "⠋ thinking", "reassembled cleanly");
        assert!(
            !partial.contains('\u{fffd}'),
            "no replacement char: {partial:?}"
        );
    }

    /// #5: a wide CJK char split across chunks keeps its width and its bytes.
    #[test]
    fn a_cjk_char_split_across_chunks_keeps_its_width() {
        let mut s = TranscriptStream::new();
        s.feed(b"ab\xe4\xb8"); // "ab" + first two bytes of 世 (E4 B8 96)
        assert_eq!(
            s.partial(),
            b"ab",
            "the half CJK char is not in the row yet"
        );
        let d = s.feed(b"\x96 world\n");
        assert_eq!(d.lines, vec!["ab世 world".as_bytes().to_vec()]);
        assert_eq!(
            visible_width(&d.lines[0]),
            2 + 2 + 6,
            "ab + 世(2) + ' world'"
        );
    }

    /// #5: a combining mark split from its base still measures zero width once
    /// reattached — the split does not inflate the column count.
    #[test]
    fn a_combining_mark_split_from_its_base_stays_zero_width() {
        let mut s = TranscriptStream::new();
        s.feed(b"e\xcc"); // 'e' + first byte of the combining acute (CC 81)
        let d = s.feed(b"\x81\n");
        assert_eq!(d.lines, vec!["e\u{301}".as_bytes().to_vec()]);
        assert_eq!(
            visible_width(&d.lines[0]),
            1,
            "base + combining is one cell"
        );
    }

    #[test]
    fn overwrite_after_carriage_return_without_erase_truncates_then_appends() {
        let mut s = TranscriptStream::new();
        s.feed(b"12345");
        s.feed(b"\rab");
        // A real terminal would show "ab345"; the sequences newt emits always
        // erase first, so the simpler truncate-then-append is documented here.
        assert_eq!(s.partial(), b"ab");
    }

    #[test]
    fn clip_keeps_leading_sgr_and_cuts_on_a_cell_boundary() {
        let row = b"\x1b[1mhello \xe4\xb8\x96\xe7\x95\x8c!\x1b[0m".to_vec();
        // "hello " (6) + 世 (2) = 8; a clip at 7 must not split 世.
        let clipped = clip_to_width(&row, 7);
        assert_eq!(clipped, b"\x1b[1mhello \x1b[0m".to_vec());
        assert_eq!(visible_width(&clipped), 6);
    }

    #[test]
    fn wrap_row_never_exceeds_cols_and_keeps_styling() {
        let row = b"\x1b[2mabcdefghij\x1b[0m".to_vec();
        let rows = wrap_row(&row, 4);
        assert_eq!(rows.len(), 3);
        for r in &rows {
            assert!(visible_width(r) <= 4, "{r:?}");
        }
        assert_eq!(rows[0], b"\x1b[2mabcd".to_vec());
        assert_eq!(rows[2], b"ij\x1b[0m".to_vec());
        // Empty in, one empty row out — an empty line still occupies a row.
        assert_eq!(wrap_row(b"", 10), vec![Vec::<u8>::new()]);
    }

    #[test]
    fn wide_glyph_wraps_before_the_margin_not_across_it() {
        let rows = wrap_row("ab世".as_bytes(), 3);
        assert_eq!(rows, vec![b"ab".to_vec(), "世".as_bytes().to_vec()]);
    }
}
