//! Rich inline-TUI input surface (issue #416) — behind the `rich-tui` feature.
//!
//! This is the production port of `examples/rich_tui_spike.rs`, reshaped to the
//! [`InputSurface`](crate::InputSurface) contract so `run_chat` can drive it the
//! same way it drives the rustyline path. It renders a ratatui
//! `Viewport::Inline` region pinned to the bottom of the terminal — **no
//! alternate screen** — so submitted turns and model output flow into real
//! scrollback. On a TTY (and only when the `rich-tui` feature is compiled in) it
//! replaces the rustyline surface; everywhere else (piped, headless, wyvern) the
//! rustyline surface still handles input.
//!
//! ## Submit semantics (parity with the rustyline path)
//! - **Enter** submits — unless the line is mid-continuation
//!   ([`footer_continues`](crate::footer_continues): a `! …\` host-shell line or
//!   an open `"""`/`'''` block), in which case Enter adds a line. This reuses
//!   the *exact* classifier the rustyline validator uses, so multi-line entry
//!   behaves identically across both surfaces.
//! - **Shift-Enter** inserts a newline in every mode (terminal permitting).
//!   **Ctrl-O** inserts a newline only in the **modeless** modes (emacs/nano),
//!   where it is idiomatic (emacs `open-line`). In **vi**, Ctrl-O is left free
//!   for its real semantics (jumplist back in NORMAL, insert-normal in INSERT);
//!   vi users open lines with `o`/`O`.
//! - **Ctrl-C** interrupts; **Ctrl-D** on an empty buffer is EOF (both exit
//!   cleanly, as in rustyline).
//! - Vi mode adds `:w`/`:wq`/`:x` (submit) and `:q`/`:q!` (quit) ex-commands.
//!
//! ## Vi gaps (future faithful-keymap work, issue #416 follow-up)
//! - **Ctrl-O / Ctrl-I jumplist** (NORMAL: jump back/forward through cursor
//!   history; `:jumps` to view) — currently a no-op in vi.
//! - **i_CTRL-O insert-normal** (INSERT: run one Normal command then resume
//!   INSERT, e.g. `Ctrl-O $`) — currently a no-op in vi.
//! - Also: `f/F/t/T/;/,` char-search, `.` repeat, `R` overwrite, exact `P`.
//!
//! ## Not yet (documented limitations of v1)
//! - No in-session history recall (Up/Down navigate the buffer, not history);
//!   submitted entries are still **persisted** to the shared history file so the
//!   rustyline path sees them next session.
//! - The status row shows the live clock + edit mode only; model / plan-mode
//!   tokens land with the status-row work (issue #416 follow-up).
//! - The per-turn event loop (`read_line`) needs a real TTY and is exercised by
//!   creature-testing, not unit tests; the editing/state logic below is fully
//!   unit-tested.

use std::io::{self, Stdout, Write as _};
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use tui_textarea::{CursorMove, TextArea};

use crate::{footer_continues, InputSurface, ReadOutcome};

const GUTTER_W: u16 = 20; // "[HH:MM:SS] NORMAL ❯ "
const MAX_INPUT_ROWS: u16 = 8;
/// Auto-gutter threshold: use the left gutter only while it stays under this
/// fraction of the terminal width; on a squished terminal, drop it and stack
/// the prompt on its own line. (A `[tui] gutter = auto|on|off` setting later.)
const GUTTER_MAX_FRACTION: f32 = 0.33;

type Term = Terminal<CrosstermBackend<Stdout>>;

fn use_gutter(width: u16) -> bool {
    width > 0 && (GUTTER_W as f32) <= GUTTER_MAX_FRACTION * width as f32
}

/// Resolve the effective gutter / input-indent width (columns) from the
/// `[tui] gutter` setting and the terminal width:
/// - `None` (auto): a prompt-width gutter (`GUTTER_W`) when it stays under ~1/3
///   of the width, else `0` (stacked prompt).
/// - `Some(n)`: exactly `n`, clamped so it can't consume the whole line.
///
/// A result `>= GUTTER_W` is wide enough to hold the inline prompt; a smaller
/// result (including `0`) stacks the prompt on its own row and indents the input
/// that many columns.
fn resolve_gutter(setting: Option<u16>, width: u16) -> u16 {
    match setting {
        None => {
            if use_gutter(width) {
                GUTTER_W
            } else {
                0
            }
        }
        Some(n) => n.min(width.saturating_sub(1)),
    }
}

/// One step of the editor: what the loop should do after handling a key.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Step {
    /// Keep editing.
    Continue,
    /// Accept the buffer as this turn's input.
    Submit,
    /// Ctrl-C — interrupt (clean exit).
    Interrupt,
    /// Ctrl-D on empty / `:q` — end of input (clean exit).
    Eof,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    Insert,
}

#[derive(Clone, Copy, PartialEq)]
enum Pending {
    None,
    Op(char),
    Replace,
    G,
}

/// The vi state machine — a faithful subset of rustyline's `vi_command`, ported
/// onto tui-textarea. NORMAL/INSERT · `h l j k w b e 0 ^ $ G gg` (counts) ·
/// `i I a A o O` · `x X D C s S r{c} u Ctrl-R p` · `d/c/y{motion}` + `dd/cc/yy`.
struct Vi {
    mode: Mode,
    pending: Pending,
    count: usize,
    /// `:`-command line buffer (`:wq`, `:q`, …); `Some` while active.
    ex: Option<String>,
}

impl Vi {
    fn new() -> Self {
        Self {
            mode: Mode::Insert,
            pending: Pending::None,
            count: 0,
            ex: None,
        }
    }

    fn take_count(&mut self) -> usize {
        let n = self.count.max(1);
        self.count = 0;
        n
    }

    /// The `:`-command line: `:w`/`:wq`/`:x` submit, `:q`/`:q!` quit. Esc or
    /// backspacing past the `:` cancels.
    fn ex_input(&mut self, key: KeyEvent) -> Step {
        match key.code {
            KeyCode::Esc => self.ex = None,
            KeyCode::Enter => {
                let cmd = self.ex.take().unwrap_or_default();
                return match cmd.as_str() {
                    "w" | "wq" | "x" | "wq!" | "x!" => Step::Submit,
                    "q" | "q!" => Step::Eof,
                    _ => Step::Continue, // unknown command just cancels
                };
            }
            KeyCode::Backspace => {
                if let Some(ex) = self.ex.as_mut() {
                    if ex.pop().is_none() {
                        self.ex = None; // backspaced past the `:`
                    }
                }
            }
            KeyCode::Char(c) => {
                if let Some(ex) = self.ex.as_mut() {
                    ex.push(c);
                }
            }
            _ => {}
        }
        Step::Continue
    }

    fn input(&mut self, key: KeyEvent, ta: &mut TextArea) -> Step {
        if self.ex.is_some() {
            return self.ex_input(key);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
            if self.mode == Mode::Normal {
                ta.redo();
            }
            return Step::Continue;
        }
        match self.mode {
            Mode::Insert => {
                if key.code == KeyCode::Esc {
                    self.mode = Mode::Normal;
                    ta.move_cursor(CursorMove::Back);
                } else {
                    ta.input(key);
                }
                Step::Continue
            }
            Mode::Normal => self.normal(key, ta),
        }
    }

    fn normal(&mut self, key: KeyEvent, ta: &mut TextArea) -> Step {
        let KeyCode::Char(c) = key.code else {
            match key.code {
                KeyCode::Left | KeyCode::Backspace => ta.move_cursor(CursorMove::Back),
                KeyCode::Right => ta.move_cursor(CursorMove::Forward),
                KeyCode::Up => ta.move_cursor(CursorMove::Up),
                KeyCode::Down => ta.move_cursor(CursorMove::Down),
                _ => {}
            }
            return Step::Continue;
        };
        match self.pending {
            Pending::Replace => {
                ta.delete_next_char();
                ta.insert_char(c);
                ta.move_cursor(CursorMove::Back);
                self.pending = Pending::None;
                return Step::Continue;
            }
            Pending::G => {
                self.pending = Pending::None;
                if c == 'g' {
                    ta.move_cursor(CursorMove::Top);
                }
                return Step::Continue;
            }
            Pending::Op(op) => {
                self.apply_operator(op, c, ta);
                self.pending = Pending::None;
                return Step::Continue;
            }
            Pending::None => {}
        }
        if c.is_ascii_digit() && !(c == '0' && self.count == 0) {
            self.count = self.count.saturating_mul(10) + (c as usize - '0' as usize);
            return Step::Continue;
        }
        if is_motion(c) {
            let n = self.take_count();
            apply_motion(ta, c, n);
            return Step::Continue;
        }
        let n = self.take_count();
        match c {
            'i' => self.mode = Mode::Insert,
            'I' => {
                ta.move_cursor(CursorMove::Head);
                self.mode = Mode::Insert;
            }
            'a' => {
                ta.move_cursor(CursorMove::Forward);
                self.mode = Mode::Insert;
            }
            'A' => {
                ta.move_cursor(CursorMove::End);
                self.mode = Mode::Insert;
            }
            'o' => {
                ta.move_cursor(CursorMove::End);
                ta.insert_newline();
                self.mode = Mode::Insert;
            }
            'O' => {
                ta.move_cursor(CursorMove::Head);
                ta.insert_newline();
                ta.move_cursor(CursorMove::Up);
                self.mode = Mode::Insert;
            }
            'x' => {
                for _ in 0..n {
                    ta.delete_next_char();
                }
            }
            'X' => {
                for _ in 0..n {
                    ta.delete_char();
                }
            }
            'D' => {
                ta.delete_line_by_end();
            }
            'C' => {
                ta.delete_line_by_end();
                self.mode = Mode::Insert;
            }
            's' => {
                for _ in 0..n {
                    ta.delete_next_char();
                }
                self.mode = Mode::Insert;
            }
            'S' => {
                ta.move_cursor(CursorMove::Head);
                ta.delete_line_by_end();
                self.mode = Mode::Insert;
            }
            'r' => self.pending = Pending::Replace,
            'u' => {
                ta.undo();
            }
            'p' | 'P' => {
                ta.paste();
            }
            'd' | 'c' | 'y' => self.pending = Pending::Op(c),
            'g' => self.pending = Pending::G,
            ':' => self.ex = Some(String::new()),
            _ => {}
        }
        Step::Continue
    }

    fn apply_operator(&mut self, op: char, target: char, ta: &mut TextArea) {
        if target == op {
            match op {
                'c' => {
                    ta.move_cursor(CursorMove::Head);
                    ta.delete_line_by_end();
                    self.mode = Mode::Insert;
                }
                'd' => {
                    ta.move_cursor(CursorMove::Head);
                    ta.delete_line_by_end();
                    ta.delete_next_char();
                }
                'y' => {
                    let start = ta.cursor();
                    ta.move_cursor(CursorMove::Head);
                    ta.start_selection();
                    ta.move_cursor(CursorMove::End);
                    ta.copy();
                    ta.cancel_selection();
                    ta.move_cursor(CursorMove::Jump(start.0 as u16, start.1 as u16));
                }
                _ => {}
            }
            return;
        }
        if !is_motion(target) {
            return;
        }
        let start = ta.cursor();
        ta.start_selection();
        apply_motion(ta, target, 1);
        match op {
            'd' => {
                ta.cut();
            }
            'c' => {
                ta.cut();
                self.mode = Mode::Insert;
            }
            'y' => {
                ta.copy();
                ta.cancel_selection();
                ta.move_cursor(CursorMove::Jump(start.0 as u16, start.1 as u16));
            }
            _ => ta.cancel_selection(),
        }
    }

    fn mode_label(&self) -> &'static str {
        match self.mode {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
        }
    }
}

fn is_motion(c: char) -> bool {
    matches!(
        c,
        'h' | 'l' | 'j' | 'k' | 'w' | 'W' | 'b' | 'B' | 'e' | 'E' | '0' | '^' | '$' | 'G'
    )
}

fn apply_motion(ta: &mut TextArea, c: char, n: usize) {
    let mv = match c {
        'h' => CursorMove::Back,
        'l' => CursorMove::Forward,
        'j' => CursorMove::Down,
        'k' => CursorMove::Up,
        'w' | 'W' => CursorMove::WordForward,
        'b' | 'B' => CursorMove::WordBack,
        'e' | 'E' => CursorMove::WordEnd,
        '0' | '^' => CursorMove::Head,
        '$' => CursorMove::End,
        'G' => CursorMove::Bottom,
        _ => return,
    };
    for _ in 0..n {
        ta.move_cursor(mv);
    }
}

/// The editor mode. **Default is Emacs** (tui-textarea's native, emacs/nano-ish
/// bindings); Vi is opt-in via `[tui] edit_mode` / `/vi`. `Nano` is modeless and
/// behaves like Emacs today — it differs only in label. Read from the same
/// source as the rustyline path ([`crate::resolve_edit_mode`]).
#[derive(Clone, Copy, PartialEq)]
enum Edit {
    Emacs,
    Nano,
    Vi,
}

impl Edit {
    /// Whether this mode uses tui-textarea's native (modeless, emacs-style)
    /// bindings — true for both Emacs and Nano.
    fn is_modeless(self) -> bool {
        matches!(self, Self::Emacs | Self::Nano)
    }
}

fn current_edit() -> Edit {
    match crate::resolve_edit_mode() {
        newt_core::EditMode::Vi => Edit::Vi,
        newt_core::EditMode::Nano => Edit::Nano,
        newt_core::EditMode::Emacs => Edit::Emacs,
    }
}

/// Per-turn editor: the edit mode plus (for vi) the mode/operator state.
struct Editor {
    edit: Edit,
    vi: Vi,
}

impl Editor {
    fn new(edit: Edit) -> Self {
        Self {
            edit,
            vi: Vi::new(),
        }
    }

    /// Handle one key. Submit / interrupt / EOF and the continuation-aware Enter
    /// are shared by both modes so behavior matches the rustyline validator.
    fn input(&mut self, key: KeyEvent, ta: &mut TextArea) -> Step {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl {
            match key.code {
                KeyCode::Char('c') => return Step::Interrupt,
                KeyCode::Char('d') => {
                    return if buffer_is_empty(ta) {
                        Step::Eof
                    } else {
                        Step::Submit
                    };
                }
                // Ctrl-O inserts a newline ONLY in the modeless modes
                // (emacs/nano) — in emacs this is idiomatic (`open-line`). In
                // vi, Ctrl-O is the jumplist "jump back" command (Ctrl-I forward,
                // `:jumps` to view), so we must NOT hijack it; vi users open
                // lines with `o`/`O` (and Shift-Enter still works). The jumplist
                // itself is a documented vi gap (TODO), so Ctrl-O is a no-op in
                // vi for now rather than wrongly inserting a newline.
                KeyCode::Char('o') if self.edit.is_modeless() => {
                    ta.insert_newline();
                    return Step::Continue;
                }
                _ => {}
            }
        }
        // Shift-Enter — explicit newline without submitting (terminal
        // permitting: many terminals send a bare CR, indistinguishable from
        // Enter, so Ctrl-O is the reliable fallback). Shared across all edit
        // modes since this runs before the per-mode dispatch.
        //
        // Ctrl-Enter is deliberately NOT bound: on macOS terminals Ctrl-Return
        // is intercepted at the terminal/OS layer (it opens a popup) and never
        // reaches us cleanly, so it is unusable cross-platform. Ctrl-O is the
        // portable newline key.
        if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT) {
            ta.insert_newline();
            return Step::Continue;
        }
        // Plain Enter: submit, unless mid-continuation, and not while a vi
        // `:`-command line is being typed (that Enter executes the command).
        if key.code == KeyCode::Enter && self.ex().is_none() {
            let body = ta.lines().join("\n");
            if footer_continues(&body) {
                ta.insert_newline();
                return Step::Continue;
            }
            return Step::Submit;
        }
        if self.edit.is_modeless() {
            // Emacs / nano: hand the key to tui-textarea's built-in bindings.
            ta.input(key);
            return Step::Continue;
        }
        self.vi.input(key, ta)
    }

    fn label(&self) -> &'static str {
        match self.edit {
            Edit::Emacs => "emacs",
            Edit::Nano => "nano",
            Edit::Vi => self.vi.mode_label(),
        }
    }

    fn ex(&self) -> Option<&str> {
        if self.edit == Edit::Vi {
            self.vi.ex.as_deref()
        } else {
            None
        }
    }
}

fn buffer_is_empty(ta: &TextArea) -> bool {
    ta.lines().iter().all(|l| l.is_empty())
}

fn make_terminal(height: u16) -> io::Result<Term> {
    Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
}

fn new_textarea(edit: Edit) -> TextArea<'static> {
    let mut ta = TextArea::default();
    // Mode-aware hint: in vi, Ctrl-O is reserved (jumplist / insert-normal), so
    // we advertise the vi-native `o`/`O` for opening lines, not Ctrl-O.
    let hint = if edit == Edit::Vi {
        "type…  (Esc=NORMAL · o/O open line · Shift-Enter newline · Enter submit · Ctrl-C quit)"
    } else {
        "type…  (Enter submit · Ctrl-O / Shift-Enter newline · Ctrl-C quit)"
    };
    ta.set_placeholder_text(hint);
    // Block (reverse) cursor; no cursor-line underline.
    ta.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    ta.set_cursor_line_style(Style::default());
    ta
}

/// The status / prompt line. Colors favor light/high-luminance tones (the
/// accessibility default — deep dark-saturated hues lose letter detail); every
/// color maps to a `[tui.colors]` key in the production palette work.
fn prompt_line(editor: &Editor) -> Line<'static> {
    if let Some(ex) = editor.ex() {
        return Line::from(Span::styled(
            format!(":{ex}"),
            Style::default().fg(Color::White),
        ));
    }
    let clock = chrono::Local::now().format("%H:%M:%S").to_string();
    Line::from(vec![
        Span::styled(format!("[{clock}] "), Style::default().fg(Color::Gray)),
        Span::styled(
            editor.label().to_string(),
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ❯ ", Style::default().fg(Color::Rgb(255, 165, 90))),
    ])
}

fn draw(f: &mut Frame, textarea: &TextArea, editor: &Editor, gutter: Option<u16>) {
    let area = f.area();
    let prompt = prompt_line(editor);
    let g = resolve_gutter(gutter, area.width);
    if g >= GUTTER_W {
        // Wide enough to hold the inline prompt: prompt in the left gutter, input
        // to its right (continuation lines align under the input).
        let [gutter_area, input] =
            Layout::horizontal([Constraint::Length(g), Constraint::Min(1)]).areas(area);
        f.render_widget(Paragraph::new(prompt), gutter_area);
        f.render_widget(textarea, input);
    } else {
        // Narrow (incl. 0): stack the prompt on its own row, then indent the
        // input by `g` columns (0 = flush-left, the old squished behavior).
        let [prow, rest] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
        f.render_widget(Paragraph::new(prompt), prow);
        let [_pad, input] =
            Layout::horizontal([Constraint::Length(g), Constraint::Min(1)]).areas(rest);
        f.render_widget(textarea, input);
    }
}

/// Emit a submitted turn into real scrollback (above the inline region), so the
/// conversation log shows what the user typed — the inline widget itself is
/// cleared on submit.
fn echo_submitted(terminal: &mut Term, body: &str) -> io::Result<()> {
    let stamp = chrono::Local::now().format("%H:%M:%S").to_string();
    let n = body.lines().count().max(1) as u16;
    terminal.insert_before(n + 1, |buf| {
        let mut lines: Vec<Line> = Vec::new();
        for (i, l) in body.lines().enumerate() {
            let prefix = if i == 0 {
                Span::styled(format!("[{stamp}] ❯ "), Style::default().fg(Color::Gray))
            } else {
                Span::raw("            ")
            };
            lines.push(Line::from(vec![prefix, Span::raw(l.to_string())]));
        }
        Paragraph::new(lines).render(buf.area, buf);
    })
}

/// The default input surface on a TTY when the `rich-tui` feature is compiled
/// in: a ratatui inline editor implementing [`InputSurface`].
pub(crate) struct RichSurface {
    edit: Edit,
    history_path: Option<PathBuf>,
    /// Entries submitted since the last `save_history`, appended on save.
    unsaved: Vec<String>,
    /// `[tui] gutter` setting: `None` = auto, `Some(0)` = off, `Some(n)` = an
    /// n-column input indent (see [`resolve_gutter`]).
    gutter: Option<u16>,
}

impl RichSurface {
    pub(crate) fn new(history_path: Option<PathBuf>) -> anyhow::Result<Self> {
        Ok(Self {
            edit: current_edit(),
            history_path,
            unsaved: Vec::new(),
            gutter: crate::resolve_gutter_setting(),
        })
    }

    /// Run the inline event loop for a single turn. Raw mode is enabled for the
    /// duration and disabled before returning, so model output between turns
    /// prints normally into scrollback.
    fn read_turn(&self) -> io::Result<ReadOutcome> {
        enable_raw_mode()?;
        let outcome = self.event_loop();
        let _ = disable_raw_mode();
        outcome
    }

    fn event_loop(&self) -> io::Result<ReadOutcome> {
        let mut cur_h = 1u16;
        // A freshly built inline terminal has a blank back-buffer, so ratatui's
        // frame diff won't rewrite cells the new frame doesn't touch — stale
        // content from a prior turn (or the smaller pre-resize region) bleeds
        // through. `clear()` forces a full repaint of the region so every turn /
        // resize starts clean.
        let mut terminal = make_terminal(cur_h)?;
        terminal.clear()?;
        let mut textarea = new_textarea(self.edit);
        let mut editor = Editor::new(self.edit);
        loop {
            // Grow/shrink the inline viewport to the input. When the gutter is
            // too narrow to hold the inline prompt, it needs its own row on top.
            let (cols, _) = crossterm::terminal::size()?;
            let prompt_rows = if resolve_gutter(self.gutter, cols) >= GUTTER_W {
                0
            } else {
                1
            };
            let want = (textarea.lines().len() as u16 + prompt_rows)
                .clamp(1, MAX_INPUT_ROWS + prompt_rows);
            if want != cur_h {
                terminal = make_terminal(want)?;
                terminal.clear()?;
                cur_h = want;
            }
            terminal.draw(|f| draw(f, &textarea, &editor, self.gutter))?;

            // 250ms timeout drives the live clock when idle.
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match editor.input(key, &mut textarea) {
                Step::Continue => {}
                Step::Submit => {
                    let body = textarea.lines().join("\n");
                    if body.trim().is_empty() {
                        continue;
                    }
                    echo_submitted(&mut terminal, &body)?;
                    return Ok(ReadOutcome::Line(body));
                }
                Step::Interrupt => return Ok(ReadOutcome::Interrupted),
                Step::Eof => return Ok(ReadOutcome::Eof),
            }
        }
    }
}

impl InputSurface for RichSurface {
    fn read_line(&mut self, _prompt: &str) -> anyhow::Result<ReadOutcome> {
        // The rich surface renders its own status row (clock + mode), so it
        // ignores the rustyline-formatted `prompt` string for now; the model /
        // plan-mode tokens fold into the status row in the follow-up.
        Ok(self.read_turn()?)
    }

    fn add_history(&mut self, entry: &str) {
        self.unsaved.push(entry.to_string());
    }

    fn save_history(&mut self) {
        let Some(hp) = self.history_path.as_ref() else {
            return;
        };
        if self.unsaved.is_empty() {
            return;
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(hp)
        {
            for e in &self.unsaved {
                // Flatten multi-line entries so each history line is one entry,
                // staying compatible with the rustyline history file format.
                let _ = writeln!(f, "{}", e.replace('\n', " "));
            }
        }
        self.unsaved.clear();
    }

    fn reload(&mut self) -> anyhow::Result<()> {
        // A `/vi` · `/emacs` · `/nano` switch changed NEWT_EDIT_MODE; pick it up
        // (and re-read the gutter setting in case it changed too).
        self.edit = current_edit();
        self.gutter = crate::resolve_gutter_setting();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn special(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn vi_editor() -> Editor {
        Editor::new(Edit::Vi)
    }
    fn emacs_editor() -> Editor {
        Editor::new(Edit::Emacs)
    }

    /// Drive a sequence of chars (in NORMAL-friendly contexts) and return lines.
    fn type_chars(ed: &mut Editor, ta: &mut TextArea, s: &str) {
        for c in s.chars() {
            ed.input(key(c), ta);
        }
    }

    fn nano_editor() -> Editor {
        Editor::new(Edit::Nano)
    }

    #[test]
    fn nano_is_modeless_and_labeled() {
        let mut ed = nano_editor();
        let mut ta = TextArea::default();
        assert_eq!(ed.label(), "nano");
        // Modeless like emacs: typing inserts text, no NORMAL mode.
        type_chars(&mut ed, &mut ta, "plain text");
        assert_eq!(ed.label(), "nano", "no mode flip");
        assert_eq!(ta.lines(), &["plain text".to_string()]);
        // Enter still submits; Ctrl-O still newlines (shared handling).
        assert_eq!(ed.input(ctrl('o'), &mut ta), Step::Continue);
        assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Submit);
        assert!(Edit::Nano.is_modeless() && Edit::Emacs.is_modeless());
        assert!(!Edit::Vi.is_modeless());
    }

    #[test]
    fn resolve_gutter_auto_off_and_fixed() {
        // auto (None): prompt-width gutter when it fits, else 0.
        assert_eq!(
            resolve_gutter(None, 80),
            GUTTER_W,
            "auto wide → inline gutter"
        );
        assert_eq!(resolve_gutter(None, 50), 0, "auto squished → stacked (0)");
        // off (Some(0)): always 0.
        assert_eq!(resolve_gutter(Some(0), 80), 0);
        // fixed N: exactly N, clamped to the usable width.
        assert_eq!(resolve_gutter(Some(3), 80), 3, "3-space indent");
        assert_eq!(
            resolve_gutter(Some(25), 80),
            25,
            "wide enough to hold the prompt"
        );
        assert_eq!(resolve_gutter(Some(200), 80), 79, "clamped to width-1");
    }

    #[test]
    fn use_gutter_drops_when_over_a_third() {
        // Keep the gutter while 20 <= 0.33*width, i.e. width >= ~61 cols.
        assert!(use_gutter(80), "gutter fits at 80 cols");
        assert!(use_gutter(61), "20 <= 0.33*61 (20.13) → gutter stays on");
        assert!(!use_gutter(60), "20 > 0.33*60 (19.8) → drop the gutter");
        assert!(!use_gutter(50), "20/50 == 0.40 → drop the gutter");
        assert!(!use_gutter(0), "zero width never uses a gutter");
    }

    #[test]
    fn emacs_enter_submits_and_ctrl_o_inserts_newline() {
        let mut ed = emacs_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "hello");
        // Ctrl-O adds a line without submitting.
        assert_eq!(ed.input(ctrl('o'), &mut ta), Step::Continue);
        type_chars(&mut ed, &mut ta, "world");
        assert_eq!(ta.lines().len(), 2, "two lines after Ctrl-O");
        // Plain Enter submits.
        assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Submit);
    }

    #[test]
    fn shift_enter_inserts_newline_without_submitting() {
        let nl = || KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        let mut ed = emacs_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "line one");
        assert_eq!(
            ed.input(nl(), &mut ta),
            Step::Continue,
            "Shift-Enter newline"
        );
        type_chars(&mut ed, &mut ta, "line two");
        assert_eq!(ta.lines().len(), 2, "Shift-Enter added a line");

        // Same in vi INSERT mode (shared handling runs before mode dispatch).
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "vi line");
        assert_eq!(ed.input(nl(), &mut ta), Step::Continue);
        assert_eq!(ta.lines().len(), 2, "Shift-Enter newline in vi too");

        // Ctrl-Enter is NOT bound (macOS intercepts it at the terminal layer);
        // a plain Enter with no continuation still submits, unaffected.
        let mut ed = emacs_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "x");
        assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Submit);
    }

    #[test]
    fn ctrl_o_is_newline_in_modeless_but_reserved_in_vi() {
        // Emacs / nano: Ctrl-O inserts a newline (idiomatic open-line).
        for ed_factory in [emacs_editor as fn() -> Editor, nano_editor] {
            let mut ed = ed_factory();
            let mut ta = TextArea::default();
            type_chars(&mut ed, &mut ta, "a");
            assert_eq!(ed.input(ctrl('o'), &mut ta), Step::Continue);
            assert_eq!(ta.lines().len(), 2, "Ctrl-O newline in modeless mode");
        }
        // Vi: Ctrl-O is reserved (jumplist / insert-normal) — it must NOT insert
        // a newline. In INSERT it is currently a no-op (a documented gap), so the
        // buffer stays a single line; vi users open lines with `o`/`O`.
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "vi");
        assert_eq!(ed.input(ctrl('o'), &mut ta), Step::Continue);
        assert_eq!(ta.lines().len(), 1, "Ctrl-O does NOT newline in vi");
    }

    #[test]
    fn enter_continues_an_open_bang_line() {
        let mut ed = emacs_editor();
        let mut ta = TextArea::default();
        // A `! …\` host-shell line is mid-continuation → Enter adds a line.
        type_chars(&mut ed, &mut ta, "! ls \\");
        assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Continue);
        assert_eq!(ta.lines().len(), 2, "Enter continued the bang line");
    }

    #[test]
    fn ctrl_c_interrupts_and_ctrl_d_empty_is_eof() {
        let mut ed = emacs_editor();
        let mut ta = TextArea::default();
        assert_eq!(ed.input(ctrl('c'), &mut ta), Step::Interrupt);
        assert_eq!(
            ed.input(ctrl('d'), &mut ta),
            Step::Eof,
            "Ctrl-D empty → EOF"
        );
        // Ctrl-D with content submits instead.
        type_chars(&mut ed, &mut ta, "x");
        assert_eq!(ed.input(ctrl('d'), &mut ta), Step::Submit);
    }

    #[test]
    fn vi_o_opens_line_below_and_enters_insert() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        // Start in INSERT (vi default), type, Esc to NORMAL.
        type_chars(&mut ed, &mut ta, "first");
        ed.input(special(KeyCode::Esc), &mut ta);
        assert_eq!(ed.label(), "NORMAL");
        // `o` opens a line below and returns to INSERT.
        ed.input(key('o'), &mut ta);
        assert_eq!(ed.label(), "INSERT");
        type_chars(&mut ed, &mut ta, "second");
        assert_eq!(ta.lines(), &["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn vi_uppercase_o_opens_line_above() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "below");
        ed.input(special(KeyCode::Esc), &mut ta);
        ed.input(key('O'), &mut ta);
        type_chars(&mut ed, &mut ta, "above");
        assert_eq!(ta.lines(), &["above".to_string(), "below".to_string()]);
    }

    #[test]
    fn vi_dd_deletes_the_line() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "doomed");
        ed.input(special(KeyCode::Esc), &mut ta);
        ed.input(key('d'), &mut ta);
        ed.input(key('d'), &mut ta);
        assert_eq!(ta.lines(), &[String::new()], "dd cleared the only line");
    }

    #[test]
    fn vi_x_with_count_deletes_n_chars() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "abcdef");
        ed.input(special(KeyCode::Esc), &mut ta); // NORMAL, cursor on 'f'
        ed.input(key('0'), &mut ta); // head
        type_chars(&mut ed, &mut ta, "3x"); // delete 3 chars
        assert_eq!(ta.lines(), &["def".to_string()]);
    }

    #[test]
    fn vi_ex_wq_submits_and_q_is_eof() {
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        type_chars(&mut ed, &mut ta, "payload");
        ed.input(special(KeyCode::Esc), &mut ta);
        // `:wq` → submit.
        ed.input(key(':'), &mut ta);
        assert_eq!(ed.ex(), Some(""), "ex line is active");
        ed.input(key('w'), &mut ta);
        ed.input(key('q'), &mut ta);
        assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Submit);

        // `:q` → EOF.
        let mut ed = vi_editor();
        let mut ta = TextArea::default();
        ed.input(special(KeyCode::Esc), &mut ta);
        ed.input(key(':'), &mut ta);
        ed.input(key('q'), &mut ta);
        assert_eq!(ed.input(special(KeyCode::Enter), &mut ta), Step::Eof);
    }

    #[test]
    fn history_appends_unsaved_entries_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let hp = dir.path().join("history");
        let mut s = RichSurface::new(Some(hp.clone())).unwrap();
        s.add_history("alpha");
        s.add_history("multi\nline");
        s.save_history();
        let contents = std::fs::read_to_string(&hp).unwrap();
        assert!(contents.contains("alpha"));
        assert!(
            contents.contains("multi line"),
            "newlines flattened to keep one entry per line"
        );
        // Second save with nothing new is a no-op (no duplicate append).
        s.save_history();
        assert_eq!(std::fs::read_to_string(&hp).unwrap(), contents);
    }

    #[test]
    fn history_without_path_is_a_noop() {
        let mut s = RichSurface::new(None).unwrap();
        s.add_history("ephemeral");
        s.save_history(); // must not panic
    }
}
