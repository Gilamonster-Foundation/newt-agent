//! Block-aware streaming writer (Step 25.3).
//!
//! The agentic loop streams the model's reply token-by-token. We cannot render
//! a half-open code fence or a table mid-arrival, so this writer line-buffers:
//! a *completed* inline line is rendered and emitted immediately (so the stream
//! still feels live), while a multi-line block construct — fenced code, GFM
//! table, list, blockquote — is **held** until it closes, then rendered whole
//! via [`render_markdown`]. Holding to completed lines also dissolves the
//! split-inline-marker problem: a `**` split across two tokens reunites in the
//! line buffer before the line is ever rendered.
//!
//! With `color == false` this is a byte-for-byte passthrough — identical to the
//! raw token stream today.

use super::{render_markdown, RenderOpts};
use std::io::{self, Write};

/// A multi-line block being accumulated until it closes.
#[derive(Clone, PartialEq)]
enum Hold {
    /// Inside a fenced code block; carries the opening fence run (``` / ~~~).
    Fence(String),
    /// Consecutive blockquote (`>`) lines.
    Quote,
    /// A list and its (indented) continuations.
    List,
    /// A pipe table. `confirmed` flips true once the delimiter row is seen;
    /// until then the opening line might just be a paragraph containing `|`.
    Table { confirmed: bool },
}

/// Streams Markdown to `out`, rendering block-by-block as content completes.
pub struct MarkdownStreamWriter<W: Write> {
    out: W,
    color: bool,
    cols: usize,
    /// The current line being accumulated (no trailing newline yet).
    line_buf: String,
    /// The multi-line block currently held open, if any.
    hold: Option<Hold>,
    /// Raw source of the held block.
    block_buf: String,
}

impl<W: Write> MarkdownStreamWriter<W> {
    pub fn new(out: W, opts: RenderOpts) -> Self {
        Self {
            out,
            color: opts.color,
            cols: opts.cols.max(8),
            line_buf: String::new(),
            hold: None,
            block_buf: String::new(),
        }
    }

    /// Feed a token delta. Completed lines are processed; the trailing partial
    /// line stays buffered until its newline arrives (or [`finish`]).
    pub fn push(&mut self, delta: &str) -> io::Result<()> {
        if !self.color {
            return self.out.write_all(delta.as_bytes());
        }
        self.line_buf.push_str(delta);
        while let Some(nl) = self.line_buf.find('\n') {
            let line: String = self.line_buf.drain(..=nl).collect();
            // Drop the trailing '\n' — line classifiers want the bare line.
            let line = line.trim_end_matches('\n').to_string();
            self.consume_line(line)?;
        }
        Ok(())
    }

    /// Give the sink back after [`finish`] (`BufWriter::into_inner`'s shape,
    /// minus the error case — nothing is buffered here once `finish` has run).
    ///
    /// #123: the streamed-answer path hands its output sink to this writer and
    /// needs it back to print the cut-stream notice *below* the answer, on the
    /// same sink. Taking the sink back is what keeps ownership single —
    /// the alternative is a second handle to the same stream, which is how
    /// two writers end up interleaving on one terminal line.
    pub fn into_inner(self) -> W {
        self.out
    }

    /// Flush the trailing partial line and any still-open block at end of stream.
    pub fn finish(&mut self) -> io::Result<()> {
        if self.color && !self.line_buf.is_empty() {
            let line = std::mem::take(&mut self.line_buf);
            self.consume_line(line)?;
        }
        match self.hold.clone() {
            // An opening `|` line that never got its delimiter row is just a
            // paragraph — emit the buffered lines as inline.
            Some(Hold::Table { confirmed: false }) => {
                let buf = std::mem::take(&mut self.block_buf);
                self.hold = None;
                for l in buf.lines() {
                    self.emit_inline(l)?;
                }
            }
            Some(_) => self.flush_block()?,
            None => {}
        }
        self.out.flush()
    }

    fn consume_line(&mut self, line: String) -> io::Result<()> {
        // `line` may need re-processing in a fresh state after a block flushes
        // (a non-matching line both *ends* a block and *starts* whatever's next).
        let mut pending = Some(line);
        while let Some(line) = pending.take() {
            match self.hold.clone() {
                Some(Hold::Fence(marker)) => {
                    self.append_block(&line);
                    if closes_fence(&line, &marker) {
                        self.flush_block()?;
                    }
                }
                Some(Hold::Quote) => {
                    if is_quote(&line) {
                        self.append_block(&line);
                    } else {
                        self.flush_block()?;
                        pending = Some(line);
                    }
                }
                Some(Hold::List) => {
                    if is_blank(&line) {
                        self.flush_block()?;
                        self.out.write_all(b"\n")?;
                    } else if is_list(&line) || is_indented(&line) {
                        self.append_block(&line);
                    } else {
                        self.flush_block()?;
                        pending = Some(line);
                    }
                }
                Some(Hold::Table { confirmed: false }) => {
                    if is_delimiter_row(&line) {
                        self.append_block(&line);
                        self.hold = Some(Hold::Table { confirmed: true });
                    } else {
                        // Not a table after all — emit the opener as inline.
                        let buf = std::mem::take(&mut self.block_buf);
                        self.hold = None;
                        for l in buf.lines() {
                            self.emit_inline(l)?;
                        }
                        pending = Some(line);
                    }
                }
                Some(Hold::Table { confirmed: true }) => {
                    if has_pipe(&line) && !is_blank(&line) {
                        self.append_block(&line);
                    } else {
                        self.flush_block()?;
                        pending = Some(line);
                    }
                }
                None => self.classify(line)?,
            }
        }
        Ok(())
    }

    /// Classify a line with no block open: start a hold, or emit it inline.
    fn classify(&mut self, line: String) -> io::Result<()> {
        if is_blank(&line) {
            self.out.write_all(b"\n")?;
        } else if let Some(marker) = fence_marker(&line) {
            self.start_hold(Hold::Fence(marker), &line);
        } else if is_quote(&line) {
            self.start_hold(Hold::Quote, &line);
        } else if is_list(&line) {
            self.start_hold(Hold::List, &line);
        } else if has_pipe(&line) {
            self.start_hold(Hold::Table { confirmed: false }, &line);
        } else {
            self.emit_inline(&line)?;
        }
        Ok(())
    }

    fn start_hold(&mut self, kind: Hold, line: &str) {
        self.hold = Some(kind);
        self.block_buf.clear();
        self.append_block(line);
    }

    fn append_block(&mut self, line: &str) {
        self.block_buf.push_str(line);
        self.block_buf.push('\n');
    }

    /// Render and emit the held block, then clear the hold.
    fn flush_block(&mut self) -> io::Result<()> {
        let block = std::mem::take(&mut self.block_buf);
        self.hold = None;
        let rendered = render_markdown(
            &block,
            RenderOpts {
                color: true,
                cols: self.cols,
            },
        );
        self.out.write_all(rendered.as_bytes())?;
        self.out.write_all(b"\n")
    }

    /// Render and emit one inline (single-line) construct.
    fn emit_inline(&mut self, line: &str) -> io::Result<()> {
        let rendered = render_markdown(
            line,
            RenderOpts {
                color: true,
                cols: self.cols,
            },
        );
        self.out.write_all(rendered.as_bytes())?;
        self.out.write_all(b"\n")
    }
}

// ---- line classifiers ----

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn is_quote(line: &str) -> bool {
    line.trim_start().starts_with('>')
}

fn is_indented(line: &str) -> bool {
    line.starts_with(' ') || line.starts_with('\t')
}

fn has_pipe(line: &str) -> bool {
    line.contains('|')
}

/// A bullet (`- * +`) or ordered (`1.` / `1)`) list item.
fn is_list(line: &str) -> bool {
    let t = line.trim_start();
    if let Some(rest) = t.strip_prefix(['-', '*', '+']) {
        return rest.is_empty() || rest.starts_with(' ');
    }
    let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() {
        if let Some(rest) = t[digits.len()..].strip_prefix(['.', ')']) {
            return rest.is_empty() || rest.starts_with(' ');
        }
    }
    false
}

/// The opening run of a code fence (``` or ~~~, length ≥ 3), if any.
fn fence_marker(line: &str) -> Option<String> {
    let t = line.trim_start();
    for ch in ['`', '~'] {
        let run: String = t.chars().take_while(|&c| c == ch).collect();
        if run.len() >= 3 {
            return Some(run);
        }
    }
    None
}

/// Whether `line` closes a fence opened with `marker`.
fn closes_fence(line: &str, marker: &str) -> bool {
    let t = line.trim();
    let ch = match marker.chars().next() {
        Some(c) => c,
        None => return false,
    };
    let run = t.chars().take_while(|&c| c == ch).count();
    run >= marker.len() && !t.is_empty() && t.chars().all(|c| c == ch)
}

/// A GFM table delimiter row: only `| - : space`, with at least one `-`.
fn is_delimiter_row(line: &str) -> bool {
    let t = line.trim();
    t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

#[cfg(test)]
mod tests {
    use super::super::RenderOpts;
    use super::MarkdownStreamWriter;

    /// Stream `src` through the writer in `chunk`-sized byte slices.
    fn stream(src: &str, chunk: usize, color: bool, cols: usize) -> String {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut w = MarkdownStreamWriter::new(&mut buf, RenderOpts { color, cols });
            let bytes = src.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                let end = (i + chunk).min(bytes.len());
                // Respect UTF-8 boundaries.
                let mut e = end;
                while e < bytes.len() && (bytes[e] & 0xC0) == 0x80 {
                    e += 1;
                }
                w.push(std::str::from_utf8(&bytes[i..e]).unwrap()).unwrap();
                i = e;
            }
            w.finish().unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    const BOLD: &str = "\x1b[1m";
    const RESET: &str = "\x1b[0m";
    const FADE: &str = "\x1b[38;2;90;90;90m";

    #[test]
    fn inline_line_renders_per_line() {
        // Whatever the chunking, a completed inline line renders identically.
        for chunk in [1, 2, 3, 100] {
            assert_eq!(
                stream("**bold** x\n", chunk, true, 80),
                format!("{BOLD}bold{RESET} x\n")
            );
        }
    }

    #[test]
    fn marker_split_across_chunks_still_bolds() {
        // The classic failure: `**` split over two tokens reunites in line_buf.
        let mut buf = Vec::new();
        {
            let mut w = MarkdownStreamWriter::new(
                &mut buf,
                RenderOpts {
                    color: true,
                    cols: 80,
                },
            );
            for d in ["a **bo", "ld** b\n"] {
                w.push(d).unwrap();
            }
            w.finish().unwrap();
        }
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            format!("a {BOLD}bold{RESET} b\n")
        );
    }

    #[test]
    fn fenced_block_held_until_close() {
        let out = stream("```\nlet x = 1;\n```\nafter\n", 1, true, 80);
        assert_eq!(out, format!("  {FADE}let x = 1;{RESET}\nafter\n"));
    }

    #[test]
    fn unterminated_fence_flushes_at_finish() {
        // No dropped content even if the closing fence never arrives.
        let out = stream("```\nlet x = 1;\n", 1, true, 80);
        assert_eq!(out, format!("  {FADE}let x = 1;{RESET}\n"));
    }

    #[test]
    fn table_streamed_matches_whole_render() {
        let src = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let whole = super::super::render_markdown(
            src.trim_end(),
            RenderOpts {
                color: true,
                cols: 80,
            },
        );
        // Streamed output is the whole-render plus the writer's trailing newline.
        assert_eq!(stream(src, 1, true, 80), format!("{whole}\n"));
    }

    #[test]
    fn pipe_paragraph_without_delimiter_is_not_a_table() {
        // `a | b` with no delimiter row must render as inline text, not a table.
        let out = stream("a | b\n", 1, true, 80);
        assert!(
            !out.contains('┌'),
            "no box for a non-table pipe line: {out:?}"
        );
        assert!(
            out.contains("a | b") || out.contains("a |"),
            "renders inline: {out:?}"
        );
    }

    #[test]
    fn color_off_is_raw_passthrough_for_any_chunking() {
        let src = "**x**\n| a | b |\n```\ncode\n```\n> q\n";
        for chunk in [1, 4, 1000] {
            assert_eq!(stream(src, chunk, false, 80), src);
        }
    }
}
