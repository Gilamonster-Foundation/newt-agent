//! The **lean** input surface (issue #527): a dead-simple, word-wrapped text box.
//!
//! LeanTUI is the flight / wyvern / cloud morphology — the prompt doubles as a
//! timestamped server-log line (`[2026-06-19 20:44:12] ❯ …`). This surface is
//! deliberately minimal by owner directive: typed text, basic cursor editing,
//! copy-and-paste, and ↑/↓ history. **No vi / emacs / nano editing modes** — that
//! richness lives in the rich [`RichSurface`](crate::rich_input::RichSurface). It
//! satisfies the [`InputSurface`](crate::InputSurface) contract so `run_chat`
//! drives it exactly like the other surfaces.
//!
//! On a non-TTY (piped / headless / wyvern) it bypasses raw mode and reads plain
//! lines from stdin, echoing the prompt so a captured log still reads
//! `[ts] ❯ <input>`.
//!
//! ## Scope / limitations (deliberate)
//! - One *logical* line (visually wrapped). Pasted newlines/tabs collapse to
//!   spaces — no multi-line editing here.
//! - Display width is approximated as the Unicode scalar count (one cell per
//!   char). Wide glyphs (CJK / emoji) in typed text may wrap a column early; the
//!   prompt itself is plain ASCII + `❯`, so the common case is exact.
//! - Cursor math uses relative moves; input wrapping past the bottom of a full
//!   screen is an accepted edge case for this minimal surface.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use crossterm::{cursor, queue};

use crate::{InputSurface, ReadOutcome};

/// What a key resolved to. `None` from [`apply_key`] means "buffer changed,
/// redraw"; a `Some` ends the read.
#[derive(Debug, PartialEq, Eq)]
enum Signal {
    Submit(String),
    Interrupt,
    Eof,
}

/// The pure editing state for one `read_line` — buffer, cursor (byte index into
/// `buf`), and history-navigation cursor. Kept free of terminal I/O so the whole
/// edit/keymap is unit-testable with synthetic [`KeyEvent`]s.
#[derive(Default)]
struct ReadState {
    buf: String,
    cursor: usize,
    /// `None` = editing the live buffer; `Some(i)` = viewing `history[i]`.
    hist_idx: Option<usize>,
    /// The live buffer, stashed while browsing history so ↓ past the newest
    /// entry restores what the user was typing.
    stash: String,
}

impl ReadState {
    fn insert_str(&mut self, s: &str) {
        self.buf.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    fn insert_char(&mut self, c: char) {
        self.buf.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Byte length of the char immediately before the cursor, if any.
    fn prev_char_len(&self) -> Option<usize> {
        self.buf[..self.cursor]
            .chars()
            .next_back()
            .map(char::len_utf8)
    }

    /// Byte length of the char at the cursor, if any.
    fn next_char_len(&self) -> Option<usize> {
        self.buf[self.cursor..].chars().next().map(char::len_utf8)
    }

    fn backspace(&mut self) {
        if let Some(n) = self.prev_char_len() {
            self.cursor -= n;
            self.buf.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.next_char_len().is_some() {
            self.buf.remove(self.cursor);
        }
    }

    fn left(&mut self) {
        if let Some(n) = self.prev_char_len() {
            self.cursor -= n;
        }
    }

    fn right(&mut self) {
        if let Some(n) = self.next_char_len() {
            self.cursor += n;
        }
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.buf.len();
    }

    fn set_line(&mut self, line: &str) {
        self.buf = line.to_string();
        self.cursor = self.buf.len();
    }

    fn history_up(&mut self, history: &[String]) {
        if history.is_empty() {
            return;
        }
        let next = match self.hist_idx {
            None => {
                self.stash = self.buf.clone();
                history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.hist_idx = Some(next);
        let line = history[next].clone();
        self.set_line(&line);
    }

    fn history_down(&mut self, history: &[String]) {
        match self.hist_idx {
            None => {}
            Some(i) if i + 1 < history.len() => {
                self.hist_idx = Some(i + 1);
                let line = history[i + 1].clone();
                self.set_line(&line);
            }
            Some(_) => {
                // Past the newest entry → back to the stashed live buffer.
                self.hist_idx = None;
                let stash = std::mem::take(&mut self.stash);
                self.set_line(&stash);
            }
        }
    }
}

/// Collapse a pasted blob into single-line text: control chars (newlines, tabs,
/// …) become spaces so the buffer stays one logical line.
fn sanitize_paste(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Apply one key press to the edit state. Returns `Some(Signal)` to end the read,
/// or `None` to keep going (buffer/cursor may have changed → redraw). Pure: no
/// terminal I/O, so the keymap is fully unit-testable.
fn apply_key(state: &mut ReadState, key: KeyEvent, history: &[String]) -> Option<Signal> {
    // Ignore key-release events (some terminals emit them under the kitty
    // protocol); act on press/repeat only.
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Enter => return Some(Signal::Submit(state.buf.clone())),
        KeyCode::Char('c') if ctrl => return Some(Signal::Interrupt),
        KeyCode::Char('d') if ctrl => {
            if state.buf.is_empty() {
                return Some(Signal::Eof);
            }
            state.delete();
        }
        // Plain typed text (Shift is fine; Ctrl/Alt chords are not text).
        KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
            state.insert_char(c);
        }
        KeyCode::Backspace => state.backspace(),
        KeyCode::Delete => state.delete(),
        KeyCode::Left => state.left(),
        KeyCode::Right => state.right(),
        KeyCode::Home => state.home(),
        KeyCode::End => state.end(),
        KeyCode::Up => state.history_up(history),
        KeyCode::Down => state.history_down(history),
        _ => {}
    }
    None
}

/// Pure layout: given the prompt's display width, the buffer, the cursor byte
/// index, and the terminal column count, return `(rows, cursor_row, cursor_col)`
/// for the wrapped `prompt + buf` block. One cell per char (see module note).
fn layout(prompt_w: usize, buf: &str, cursor_byte: usize, cols: usize) -> (usize, usize, usize) {
    let cols = cols.max(1);
    let before = buf[..cursor_byte.min(buf.len())].chars().count();
    let cursor_pos = prompt_w + before;
    let total = prompt_w + buf.chars().count();
    let rows = if total == 0 {
        1
    } else {
        (total - 1) / cols + 1
    };
    (rows, cursor_pos / cols, cursor_pos % cols)
}

/// RAII: restore the terminal (cooked mode, bracketed paste off) on every exit
/// path — normal return, `?`, or panic.
struct RawGuard;
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = queue!(io::stdout(), DisableBracketedPaste);
        let _ = io::stdout().flush();
        let _ = disable_raw_mode();
    }
}

/// The lean input surface: a minimal crossterm text box + ↑/↓ file-backed history.
pub(crate) struct LeanSurface {
    history: Vec<String>,
    history_path: Option<PathBuf>,
    /// Entries submitted since the last [`save_history`](InputSurface::save_history),
    /// appended on save (rustyline-compatible: one line per entry, oldest first).
    pending: Vec<String>,
    /// Rows the previous render placed *above* the cursor — so the next render
    /// can move back to the block's first row before clearing.
    rows_above_cursor: u16,
}

impl LeanSurface {
    pub(crate) fn new(history_path: Option<PathBuf>) -> anyhow::Result<Self> {
        let history = history_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default();
        Ok(Self {
            history,
            history_path,
            pending: Vec::new(),
            rows_above_cursor: 0,
        })
    }

    /// Piped / headless path: echo the prompt + the consumed line so a captured
    /// log reads `[ts] ❯ <input>`; no raw mode.
    fn read_piped(&mut self, prompt: &str) -> io::Result<ReadOutcome> {
        let mut out = io::stdout();
        write!(out, "{prompt}")?;
        out.flush()?;
        let mut line = String::new();
        if io::stdin().lock().read_line(&mut line)? == 0 {
            writeln!(out)?;
            return Ok(ReadOutcome::Eof);
        }
        let line = line.trim_end_matches(['\n', '\r']).to_string();
        // stdin was a pipe (not echoed by a tty); surface it into the log.
        writeln!(out, "{line}")?;
        Ok(ReadOutcome::Line(line))
    }

    /// Redraw the wrapped `prompt + buf` block in place and park the cursor.
    fn render(&mut self, prompt: &str, state: &ReadState) -> io::Result<()> {
        let cols = crossterm::terminal::size()
            .map(|(c, _)| c)
            .unwrap_or(80)
            .max(1) as usize;
        let prompt_w = prompt.chars().count();
        let (_, crow, ccol) = layout(prompt_w, &state.buf, state.cursor, cols);
        let end_chars = prompt_w + state.buf.chars().count();
        // Row of the last *printed* cell (where the terminal parks the cursor
        // after auto-wrapping the final glyph — pending-wrap stays on this row).
        let erow = if end_chars == 0 {
            0
        } else {
            (end_chars - 1) / cols
        };

        let mut out = io::stdout();
        queue!(out, cursor::MoveToColumn(0))?;
        if self.rows_above_cursor > 0 {
            queue!(out, cursor::MoveUp(self.rows_above_cursor))?;
        }
        queue!(out, Clear(ClearType::FromCursorDown))?;
        write!(out, "{prompt}{}", state.buf)?;
        // Reposition from the post-print row (erow) to the cursor's row/col.
        let erow = erow as u16;
        let crow_u = crow as u16;
        if erow > crow_u {
            queue!(out, cursor::MoveUp(erow - crow_u))?;
        } else if crow_u > erow {
            queue!(out, cursor::MoveDown(crow_u - erow))?;
        }
        queue!(out, cursor::MoveToColumn(ccol as u16))?;
        out.flush()?;
        self.rows_above_cursor = crow_u;
        Ok(())
    }

    /// Interactive TTY path: raw-mode editing until Enter / Ctrl-C / Ctrl-D.
    fn read_tty(&mut self, prompt: &str) -> io::Result<ReadOutcome> {
        enable_raw_mode()?;
        let _guard = RawGuard;
        queue!(io::stdout(), EnableBracketedPaste)?;
        io::stdout().flush()?;

        let mut state = ReadState::default();
        self.rows_above_cursor = 0;
        self.render(prompt, &state)?;

        let signal = loop {
            match event::read()? {
                Event::Key(k) => {
                    if let Some(sig) = apply_key(&mut state, k, &self.history) {
                        break sig;
                    }
                    self.render(prompt, &state)?;
                }
                Event::Paste(text) => {
                    state.insert_str(&sanitize_paste(&text));
                    self.render(prompt, &state)?;
                }
                Event::Resize(..) => {
                    // Width changed → recompute from scratch (no stale row count).
                    self.rows_above_cursor = 0;
                    self.render(prompt, &state)?;
                }
                _ => {}
            }
        };

        // Leave the committed prompt+input in scrollback (it IS the log line):
        // park the cursor below the block, then drop to cooked mode.
        let cols = crossterm::terminal::size()
            .map(|(c, _)| c)
            .unwrap_or(80)
            .max(1) as usize;
        let (rows, crow, _) = layout(prompt.chars().count(), &state.buf, state.cursor, cols);
        let down = rows.saturating_sub(1).saturating_sub(crow) as u16;
        let mut out = io::stdout();
        if down > 0 {
            queue!(out, cursor::MoveDown(down))?;
        }
        write!(out, "\r\n")?;
        out.flush()?;

        Ok(match signal {
            Signal::Submit(s) => ReadOutcome::Line(s),
            Signal::Interrupt => ReadOutcome::Interrupted,
            Signal::Eof => ReadOutcome::Eof,
        })
    }
}

impl InputSurface for LeanSurface {
    fn read_line(&mut self, prompt: &str) -> anyhow::Result<ReadOutcome> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Ok(self.read_piped(prompt)?);
        }
        // EMFILE pre-check: a full fd table means enable_raw_mode / opening the
        // tty would fail; surface a clean message (parity with the rich/rustyline
        // surfaces) rather than a raw error.
        if !crate::terminal_fd_available() {
            return Ok(ReadOutcome::Fatal(
                "\nnewt: EMFILE — file descriptor table is full.\n      \
                 Too many subprocesses (e.g. cargo test workers) inherited fds.\n      \
                 Restart newt. The O_CLOEXEC fix prevents recurrence on rebuilt binaries."
                    .to_string(),
            ));
        }
        Ok(self.read_tty(prompt)?)
    }

    fn add_history(&mut self, entry: &str) {
        if entry.is_empty() {
            return;
        }
        if self.history.last().map(String::as_str) == Some(entry) {
            return; // collapse consecutive duplicates
        }
        self.history.push(entry.to_string());
        self.pending.push(entry.to_string());
    }

    fn save_history(&mut self) {
        let Some(path) = self.history_path.clone() else {
            return;
        };
        if self.pending.is_empty() {
            return;
        }
        // Append the pending entries, one per line (rustyline-compatible format).
        use std::io::Write as _;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            for e in self.pending.drain(..) {
                let _ = writeln!(f, "{e}");
            }
        }
    }

    fn reload(&mut self) -> anyhow::Result<()> {
        // Lean has no edit-mode/config state to rebuild; nothing to do.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn typ(state: &mut ReadState, s: &str) {
        for c in s.chars() {
            assert_eq!(apply_key(state, key(KeyCode::Char(c)), &[]), None);
        }
    }

    #[test]
    fn types_and_submits_on_enter() {
        let mut s = ReadState::default();
        typ(&mut s, "hello");
        assert_eq!(s.buf, "hello");
        assert_eq!(s.cursor, 5);
        assert_eq!(
            apply_key(&mut s, key(KeyCode::Enter), &[]),
            Some(Signal::Submit("hello".to_string()))
        );
    }

    #[test]
    fn backspace_and_mid_line_insert() {
        let mut s = ReadState::default();
        typ(&mut s, "helo");
        apply_key(&mut s, key(KeyCode::Left), &[]); // cursor before 'o'
        apply_key(&mut s, key(KeyCode::Char('l')), &[]); // "hello"
        assert_eq!(s.buf, "hello");
        s.end();
        apply_key(&mut s, key(KeyCode::Backspace), &[]);
        assert_eq!(s.buf, "hell");
    }

    #[test]
    fn cursor_motion_home_end_delete() {
        let mut s = ReadState::default();
        typ(&mut s, "abc");
        apply_key(&mut s, key(KeyCode::Home), &[]);
        assert_eq!(s.cursor, 0);
        apply_key(&mut s, key(KeyCode::Delete), &[]); // remove 'a'
        assert_eq!(s.buf, "bc");
        apply_key(&mut s, key(KeyCode::End), &[]);
        assert_eq!(s.cursor, s.buf.len());
    }

    #[test]
    fn unicode_cursor_is_char_aware() {
        let mut s = ReadState::default();
        typ(&mut s, "café"); // 'é' is 2 bytes
        apply_key(&mut s, key(KeyCode::Left), &[]); // before 'é'
        apply_key(&mut s, key(KeyCode::Backspace), &[]); // remove 'f'
        assert_eq!(s.buf, "caé");
    }

    #[test]
    fn ctrl_c_interrupts_ctrl_d_eofs_when_empty() {
        let mut s = ReadState::default();
        assert_eq!(apply_key(&mut s, ctrl('c'), &[]), Some(Signal::Interrupt));
        assert_eq!(apply_key(&mut s, ctrl('d'), &[]), Some(Signal::Eof));
        // Ctrl-D on a non-empty buffer deletes forward instead of EOF.
        typ(&mut s, "ab");
        s.home();
        assert_eq!(apply_key(&mut s, ctrl('d'), &[]), None);
        assert_eq!(s.buf, "b");
    }

    #[test]
    fn ctrl_chords_are_not_typed_text() {
        let mut s = ReadState::default();
        // Ctrl-A is not a vi/emacs binding here and must NOT insert 'a'.
        assert_eq!(apply_key(&mut s, ctrl('a'), &[]), None);
        assert_eq!(s.buf, "");
    }

    #[test]
    fn history_up_down_recall_and_restore_live() {
        let hist = vec!["first".to_string(), "second".to_string()];
        let mut s = ReadState::default();
        typ(&mut s, "draft");
        apply_key(&mut s, key(KeyCode::Up), &hist); // newest
        assert_eq!(s.buf, "second");
        apply_key(&mut s, key(KeyCode::Up), &hist);
        assert_eq!(s.buf, "first");
        apply_key(&mut s, key(KeyCode::Up), &hist); // clamp at oldest
        assert_eq!(s.buf, "first");
        apply_key(&mut s, key(KeyCode::Down), &hist);
        assert_eq!(s.buf, "second");
        apply_key(&mut s, key(KeyCode::Down), &hist); // past newest → live draft
        assert_eq!(s.buf, "draft");
    }

    #[test]
    fn paste_collapses_newlines_to_single_line() {
        assert_eq!(sanitize_paste("a\nb\tc\rd"), "a b c d");
        let mut s = ReadState::default();
        s.insert_str(&sanitize_paste("two\nlines"));
        assert_eq!(s.buf, "two lines");
    }

    #[test]
    fn layout_wraps_and_places_cursor() {
        // cols=10, prompt width 4 ("[t] "), buffer "abcdefgh" (8) → 12 cells → 2 rows.
        let (rows, crow, ccol) = layout(4, "abcdefgh", 8, 10);
        assert_eq!(rows, 2);
        // cursor at end: cell 12 → row 1, col 2.
        assert_eq!((crow, ccol), (1, 2));
        // empty buffer is one row; cursor sits right after the prompt.
        assert_eq!(layout(4, "", 0, 10), (1, 0, 4));
        // exact-fill: prompt 2 + buf 8 = 10 cells = 1 row; cursor at end wraps to row 1 col 0.
        let (rows, crow, ccol) = layout(2, "abcdefgh", 8, 10);
        assert_eq!((rows, crow, ccol), (1, 1, 0));
    }

    /// #1409: `layout` is the lean surface's entire wrapping model, and every
    /// assertion above pins it at `cols=10`. A width-independent identity that
    /// only holds at one width is not a tested identity — and #1408 is about to
    /// move terminal ownership underneath this function.
    ///
    /// The invariants, restated so they must hold at EVERY width: the cursor
    /// cell is `prompt_w + chars_before_cursor`; `rows` is that total cell count
    /// divided into `cols`-wide rows (empty renders as one row, not zero); and
    /// `(crow, ccol)` is exactly that cell in row-major order.
    #[test]
    fn layout_invariants_hold_at_every_width() {
        for cols in [1usize, 2, 7, 10, 13, 40, 80, 200] {
            for prompt_w in [0usize, 1, 4, 11] {
                for buf in ["", "a", "abcdefgh", "hello world, a longer buffer"] {
                    for cursor in [0usize, buf.len() / 2, buf.len()] {
                        // Only split on a char boundary; `layout` slices by byte.
                        if !buf.is_char_boundary(cursor) {
                            continue;
                        }
                        let (rows, crow, ccol) = layout(prompt_w, buf, cursor, cols);
                        let cell = prompt_w + buf[..cursor].chars().count();
                        let total = prompt_w + buf.chars().count();

                        let want_rows = if total == 0 {
                            1
                        } else {
                            (total - 1) / cols + 1
                        };
                        assert_eq!(
                            rows, want_rows,
                            "rows at cols={cols} prompt_w={prompt_w} buf={buf:?}"
                        );
                        assert_eq!(
                            (crow, ccol),
                            (cell / cols, cell % cols),
                            "cursor cell {cell} at cols={cols} prompt_w={prompt_w} buf={buf:?}"
                        );
                        assert!(ccol < cols, "column must stay inside the terminal");
                        assert!(rows >= 1, "a rendered line always occupies a row");
                    }
                }
            }
        }
    }

    /// #1409: `cols` is clamped to at least 1 (`layout`'s first line), because a
    /// zero-width terminal would otherwise divide by zero. `crossterm`'s
    /// `terminal::size()` can legitimately report 0 on a detached or freshly
    /// resized pty, and `render` feeds that straight in via `.max(1)` — this
    /// pins the guard on the pure side too.
    #[test]
    fn layout_survives_a_zero_width_terminal() {
        assert_eq!(layout(0, "", 0, 0), (1, 0, 0));
        // Every cell lands on its own row when the clamp makes cols == 1.
        // prompt(1) + "ab"(2) = 3 cells → 3 rows; the cursor sits at cell 3,
        // i.e. row 3 — one past the last occupied row. That is the PENDING-WRAP
        // position, and `crow == rows` is correct rather than off-by-one: the
        // terminal has not yet scrolled the next row into existence, and
        // `render` compensates by parking the cursor from `erow` (the last
        // printed row) rather than from `rows`. The exact-fill assertion in
        // `layout_wraps_and_places_cursor` — layout(2, "abcdefgh", 8, 10) ==
        // (1, 1, 0) — is the same situation at a realistic width.
        let (rows, crow, ccol) = layout(1, "ab", 2, 0);
        assert_eq!((rows, crow, ccol), (3, 3, 0));
    }

    #[test]
    fn add_history_skips_empty_and_consecutive_dupes() {
        let mut surf = LeanSurface::new(None).unwrap();
        surf.add_history("");
        surf.add_history("a");
        surf.add_history("a");
        surf.add_history("b");
        assert_eq!(surf.history, vec!["a".to_string(), "b".to_string()]);
    }
}
