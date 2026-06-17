//! Phase-1 spike (issue #416): prove the rich-TUI substrate AND that we can
//! emulate rustyline's vi (+ the `o`/`O` patch) EXACTLY on ratatui+tui-textarea.
//! Throwaway examples/ binary — NOT production code.
//!
//! This revision folds creature-test feedback:
//! - **block (reverse) cursor**, not an underline;
//! - status folded into a **single prompt line** `[HH:MM:SS] MODE ❯ ` (a left
//!   gutter), input to its right, so the default is one line;
//! - the inline viewport **grows** with the number of input lines (recreated
//!   on change, clamped 1..8) — blank lines now have somewhere to go.
//!
//! Substrate: ratatui `Viewport::Inline` (no alt screen — submitted lines go to
//! real scrollback via `insert_before`); live light-blue clock; non-TTY gate.
//!
//! Vi mirrors rustyline `vi_command`: NORMAL/INSERT · h l j k w b e 0 ^ $ G gg
//! (counts) · i I a A o O · x X D C s S r{c} u Ctrl-R p · d/c/y{motion} + dd/cc/yy.
//! Gaps for the port: f/F/t/T/;/, · `.` · `R` · exact `P` · big-word · op counts.
//!
//! Run on a TTY:  cargo run -p newt-tui --example rich_tui_spike

use std::io::{self, IsTerminal, Stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use tui_textarea::{CursorMove, TextArea};

const GUTTER_W: u16 = 20; // "[HH:MM:SS] NORMAL ❯ "
const MAX_INPUT_ROWS: u16 = 8;
/// In the port this is a setting (`[tui] gutter = auto|on|off`). Auto: use the
/// left gutter only while it stays under this fraction of the terminal width;
/// on a squished terminal, drop it and stack the prompt on its own line.
const GUTTER_MAX_FRACTION: f32 = 0.33;

fn use_gutter(width: u16) -> bool {
    width > 0 && (GUTTER_W as f32) <= GUTTER_MAX_FRACTION * width as f32
}

type Term = Terminal<CrosstermBackend<Stdout>>;

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

enum Outcome {
    Continue,
    Submit,
    Quit,
}

struct Vi {
    mode: Mode,
    pending: Pending,
    count: usize,
    ex: Option<String>, // `:`-command line buffer (`:wq`, `:q`, …)
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
    /// The `:`-command line (a bonus beyond rustyline's vi): `:w`/`:wq`/`:x`
    /// submit, `:q`/`:q!` quit. Esc or backspacing past `:` cancels.
    fn ex_input(&mut self, key: crossterm::event::KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => self.ex = None,
            KeyCode::Enter => {
                let cmd = self.ex.take().unwrap_or_default();
                return match cmd.as_str() {
                    "w" | "wq" | "x" | "wq!" | "x!" => Outcome::Submit,
                    "q" | "q!" => Outcome::Quit,
                    _ => Outcome::Continue, // unknown command just cancels
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
        Outcome::Continue
    }
    fn input(&mut self, key: crossterm::event::KeyEvent, ta: &mut TextArea) -> Outcome {
        if self.ex.is_some() {
            return self.ex_input(key);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => return Outcome::Quit,
                KeyCode::Char('d') => return Outcome::Submit,
                KeyCode::Char('r') if self.mode == Mode::Normal => {
                    ta.redo();
                    return Outcome::Continue;
                }
                _ => {}
            }
        }
        match self.mode {
            Mode::Insert => {
                if key.code == KeyCode::Esc {
                    self.mode = Mode::Normal;
                    ta.move_cursor(CursorMove::Back);
                } else {
                    ta.input(key);
                }
                Outcome::Continue
            }
            Mode::Normal => self.normal(key, ta),
        }
    }
    fn normal(&mut self, key: crossterm::event::KeyEvent, ta: &mut TextArea) -> Outcome {
        let KeyCode::Char(c) = key.code else {
            match key.code {
                KeyCode::Left | KeyCode::Backspace => ta.move_cursor(CursorMove::Back),
                KeyCode::Right => ta.move_cursor(CursorMove::Forward),
                KeyCode::Up => ta.move_cursor(CursorMove::Up),
                KeyCode::Down => ta.move_cursor(CursorMove::Down),
                _ => {}
            }
            return Outcome::Continue;
        };
        match self.pending {
            Pending::Replace => {
                ta.delete_next_char();
                ta.insert_char(c);
                ta.move_cursor(CursorMove::Back);
                self.pending = Pending::None;
                return Outcome::Continue;
            }
            Pending::G => {
                self.pending = Pending::None;
                if c == 'g' {
                    ta.move_cursor(CursorMove::Top);
                }
                return Outcome::Continue;
            }
            Pending::Op(op) => {
                self.apply_operator(op, c, ta);
                self.pending = Pending::None;
                return Outcome::Continue;
            }
            Pending::None => {}
        }
        if c.is_ascii_digit() && !(c == '0' && self.count == 0) {
            self.count = self.count.saturating_mul(10) + (c as usize - '0' as usize);
            return Outcome::Continue;
        }
        if is_motion(c) {
            let n = self.take_count();
            apply_motion(ta, c, n);
            return Outcome::Continue;
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
        Outcome::Continue
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
/// bindings — what most people expect); Vi is opt-in. In the real port this maps
/// to the existing `[tui] edit_mode` config + `/vi` `/emacs` toggle.
#[derive(Clone, Copy, PartialEq)]
enum Edit {
    Emacs,
    Vi,
}

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
    fn reset(&mut self) {
        self.vi = Vi::new();
    }
    fn input(&mut self, key: crossterm::event::KeyEvent, ta: &mut TextArea) -> Outcome {
        // Submit / quit are shared by both modes.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => return Outcome::Quit,
                KeyCode::Char('d') => return Outcome::Submit,
                _ => {}
            }
        }
        match self.edit {
            // Emacs/nano: hand the key straight to tui-textarea's built-in
            // (emacs-style) bindings — no modes, always inserting.
            Edit::Emacs => {
                ta.input(key);
                Outcome::Continue
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

fn main() -> io::Result<()> {
    if !io::stdout().is_terminal() {
        eprintln!("rich_tui_spike: not a TTY — the plain (rustyline) path handles this. Exiting.");
        return Ok(());
    }
    // Default emacs; `--`-free `vi` arg opts in (maps to [tui] edit_mode).
    let edit = if std::env::args().skip(1).any(|a| a == "vi") {
        Edit::Vi
    } else {
        Edit::Emacs
    };

    enable_raw_mode()?;
    let mut cur_h = 1u16;
    let mut terminal = make_terminal(cur_h)?;
    let mut textarea = new_textarea();
    let mut editor = Editor::new(edit);
    let result = run(&mut terminal, &mut cur_h, &mut textarea, &mut editor);
    disable_raw_mode()?;
    println!();
    result
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
    ta.set_placeholder_text(
        "type…  (Esc → vi NORMAL · o/O open line · Ctrl-D submit · Ctrl-C quit)",
    );
    // block (reverse) cursor; no cursor-line underline.
    ta.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    ta.set_cursor_line_style(Style::default());
    ta
}

fn run(
    terminal: &mut Term,
    cur_h: &mut u16,
    textarea: &mut TextArea,
    editor: &mut Editor,
) -> io::Result<()> {
    loop {
        // Grow/shrink the inline viewport to the input. In no-gutter mode the
        // prompt needs its own row on top.
        let (cols, _) = crossterm::terminal::size()?;
        let prompt_rows = if use_gutter(cols) { 0 } else { 1 };
        let want =
            (textarea.lines().len() as u16 + prompt_rows).clamp(1, MAX_INPUT_ROWS + prompt_rows);
        if want != *cur_h {
            *terminal = make_terminal(want)?;
            *cur_h = want;
        }
        terminal.draw(|f| draw(f, textarea, editor))?;

        if !event::poll(Duration::from_millis(250))? {
            continue; // live clock
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match editor.input(key, textarea) {
            Outcome::Quit => return Ok(()),
            Outcome::Submit => {
                submit(terminal, textarea)?;
                editor.reset();
            }
            Outcome::Continue => {}
        }
    }
}

fn submit(terminal: &mut Term, textarea: &mut TextArea) -> io::Result<()> {
    let body = textarea.lines().join("\n");
    if body.trim().is_empty() {
        return Ok(());
    }
    let stamp = chrono::Local::now().format("%H:%M:%S").to_string();
    let n = textarea.lines().len() as u16;
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
    })?;
    *textarea = new_textarea();
    Ok(())
}

fn prompt_line(editor: &Editor) -> Line<'static> {
    // `:`-command line takes over the prompt while active.
    if let Some(ex) = editor.ex() {
        return Line::from(Span::styled(
            format!(":{ex}"),
            Style::default().fg(Color::White),
        ));
    }
    let clock = chrono::Local::now().format("%H:%M:%S").to_string();
    Line::from(vec![
        // LIGHT timestamp — high luminance reads better than a deep/dark tone
        // (accessibility default; maps to `[tui.colors] dim`).
        Span::styled(format!("[{clock}] "), Style::default().fg(Color::Gray)),
        Span::styled(
            editor.label().to_string(),
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        ),
        // Lighter, warmer orange caret (not the deep brand red-orange).
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
        // Squished (gutter would exceed ~33% width): stack the prompt on its own
        // row, give the input the full width.
        let [prow, input] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
        f.render_widget(Paragraph::new(prompt), prow);
        f.render_widget(textarea, input);
    }
}
