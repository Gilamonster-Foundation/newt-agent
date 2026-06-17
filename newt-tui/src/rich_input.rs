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
//! - **Ctrl-O** / **Shift-Enter** always insert a newline (newt's existing
//!   multi-line keys).
//! - **Ctrl-C** interrupts; **Ctrl-D** on an empty buffer is EOF (both exit
//!   cleanly, as in rustyline).
//! - Vi mode adds `:w`/`:wq`/`:x` (submit) and `:q`/`:q!` (quit) ex-commands.
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

use crate::{build_rl_config, footer_continues, InputSurface, ReadOutcome};

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
/// bindings); Vi is opt-in via `[tui] edit_mode` / `/vi`. Read from the same
/// source as the rustyline path ([`build_rl_config`]).
#[derive(Clone, Copy, PartialEq)]
enum Edit {
    Emacs,
    Vi,
}

fn current_edit() -> Edit {
    if build_rl_config().edit_mode() == rustyline::config::EditMode::Vi {
        Edit::Vi
    } else {
        Edit::Emacs
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
                // Force a newline without submitting (newt's existing key).
                KeyCode::Char('o') => {
                    ta.insert_newline();
                    return Step::Continue;
                }
                _ => {}
            }
        }
        // Shift-Enter (terminal permitting) — explicit newline.
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
        match self.edit {
            // Emacs/nano: hand the key to tui-textarea's built-in bindings.
            Edit::Emacs => {
                ta.input(key);
                Step::Continue
            }
            Edit::Vi => self.vi.input(key, ta),
        }
    }

    fn label(&self) -> &'static str {
        match self.edit {
            Edit::Emacs => "emacs",
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

fn new_textarea() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_placeholder_text("type…  (Enter submit · Ctrl-O newline · Ctrl-C quit)");
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

fn draw(f: &mut Frame, textarea: &TextArea, editor: &Editor) {
    let area = f.area();
    let prompt = prompt_line(editor);
    if use_gutter(area.width) {
        // Wide enough: single-line-by-default — prompt in a left gutter, input
        // to its right (continuation lines align under the input).
        let [gutter, input] =
            Layout::horizontal([Constraint::Length(GUTTER_W), Constraint::Min(1)]).areas(area);
        f.render_widget(Paragraph::new(prompt), gutter);
        f.render_widget(textarea, input);
    } else {
        // Squished: stack the prompt on its own row, input full-width.
        let [prow, input] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
        f.render_widget(Paragraph::new(prompt), prow);
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
}

impl RichSurface {
    pub(crate) fn new(history_path: Option<PathBuf>) -> anyhow::Result<Self> {
        Ok(Self {
            edit: current_edit(),
            history_path,
            unsaved: Vec::new(),
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
        let mut terminal = make_terminal(cur_h)?;
        let mut textarea = new_textarea();
        let mut editor = Editor::new(self.edit);
        loop {
            // Grow/shrink the inline viewport to the input. In no-gutter mode the
            // prompt needs its own row on top.
            let (cols, _) = crossterm::terminal::size()?;
            let prompt_rows = if use_gutter(cols) { 0 } else { 1 };
            let want = (textarea.lines().len() as u16 + prompt_rows)
                .clamp(1, MAX_INPUT_ROWS + prompt_rows);
            if want != cur_h {
                terminal = make_terminal(want)?;
                cur_h = want;
            }
            terminal.draw(|f| draw(f, &textarea, &editor))?;

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
        // A `/vi` · `/emacs` switch changed NEWT_EDIT_MODE; pick it up.
        self.edit = current_edit();
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
