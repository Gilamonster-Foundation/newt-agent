//! Phase-1 spike (issue #416): prove the rich-TUI substrate AND that we can
//! emulate rustyline's vi (+ the `o`/`O` patch) EXACTLY on ratatui+tui-textarea.
//! Throwaway examples/ binary — NOT production code.
//!
//! Substrate proven: ratatui `Viewport::Inline` (pinned bottom region, no alt
//! screen — submitted lines scroll into real scrollback via `insert_before`);
//! a live light-blue clock; the non-TTY gate (piped → exits to the "gills").
//!
//! Vi keymap mirrors rustyline's `vi_command` command-for-command:
//!   modes: NORMAL / INSERT (Esc moves left, vi-style)
//!   motions: h l j k  w W b B e E  0 ^ $  G  gg   (counts: 3w, 5j, 2x …)
//!   enter insert: i I a A  o O
//!   edits: x X  D C  s S  r{c}  u  Ctrl-R(redo)  p
//!   operators: d{motion} c{motion} y{motion}  and doubled dd cc yy
//! Submit: Ctrl-D (scrolls above)   Quit: Ctrl-C
//!
//! Known gaps to close in the production port (documented, not faithful yet):
//!   f/F/t/T/;/, char-search · `.` repeat-change · `R` overwrite mode ·
//!   exact `P` (before) · big-word vs word distinction · counts on operators.
//!
//! Run on a TTY:  cargo run -p newt-tui --example rich_tui_spike

use std::io::{self, IsTerminal};
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

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    Insert,
}

#[derive(Clone, Copy, PartialEq)]
enum Pending {
    None,
    Op(char), // d / c / y awaiting a motion
    Replace,  // r awaiting the replacement char
    G,        // g awaiting g (for `gg`)
}

enum Outcome {
    Continue,
    Submit,
    Quit,
}

struct Vi {
    mode: Mode,
    pending: Pending,
    count: usize, // 0 = none
}

impl Vi {
    fn new() -> Self {
        Vi {
            mode: Mode::Insert,
            pending: Pending::None,
            count: 0,
        }
    }

    fn take_count(&mut self) -> usize {
        let n = self.count.max(1);
        self.count = 0;
        n
    }

    fn input(&mut self, key: crossterm::event::KeyEvent, ta: &mut TextArea) -> Outcome {
        // Global keys (both modes).
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
                    ta.move_cursor(CursorMove::Back); // vi leaves cursor on last char
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
            // map a few non-char keys to motions
            match key.code {
                KeyCode::Left | KeyCode::Backspace => ta.move_cursor(CursorMove::Back),
                KeyCode::Right => ta.move_cursor(CursorMove::Forward),
                KeyCode::Up => ta.move_cursor(CursorMove::Up),
                KeyCode::Down => ta.move_cursor(CursorMove::Down),
                _ => {}
            }
            return Outcome::Continue;
        };

        // Multi-key state: replace-char, `gg`, operator-pending.
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

        // Count prefix (a leading 0 is the motion, later 0s are digits).
        if c.is_ascii_digit() && !(c == '0' && self.count == 0) {
            self.count = self.count.saturating_mul(10) + (c as usize - '0' as usize);
            return Outcome::Continue;
        }

        // Motions move the cursor; counts repeat them.
        if is_motion(c) {
            let n = self.take_count();
            apply_motion(ta, c, n);
            return Outcome::Continue;
        }

        let n = self.take_count();
        match c {
            // enter insert
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
            // the patch: open line below / above
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
            // edits
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
            // operators + `g`
            'd' | 'c' | 'y' => self.pending = Pending::Op(c),
            'g' => self.pending = Pending::G,
            _ => {}
        }
        Outcome::Continue
    }

    /// Resolve `d{motion}` / `c{motion}` / `y{motion}` and doubled `dd`/`cc`/`yy`.
    fn apply_operator(&mut self, op: char, target: char, ta: &mut TextArea) {
        if target == op {
            // linewise
            match op {
                'c' => {
                    ta.move_cursor(CursorMove::Head);
                    ta.delete_line_by_end();
                    self.mode = Mode::Insert;
                }
                'd' => {
                    ta.move_cursor(CursorMove::Head);
                    ta.delete_line_by_end();
                    ta.delete_next_char(); // pull the next line up
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
            return; // invalid motion cancels the operator (vi-style)
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

    fn status(&self) -> String {
        let m = match self.mode {
            Mode::Normal => "-- NORMAL --",
            Mode::Insert => "-- INSERT --",
        };
        let pend = match self.pending {
            Pending::Op(c) => format!("  {c}"),
            Pending::Replace => "  r".to_string(),
            Pending::G => "  g".to_string(),
            Pending::None => String::new(),
        };
        let cnt = if self.count > 0 {
            format!("  {}", self.count)
        } else {
            String::new()
        };
        format!("{m}{pend}{cnt}")
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
        '0' | '^' => CursorMove::Head, // ^ approximates first-non-blank
        '$' => CursorMove::End,
        'G' => CursorMove::Bottom,
        _ => return,
    };
    for _ in 0..n {
        ta.move_cursor(mv);
    }
}

fn main() -> io::Result<()> {
    if !io::stdout().is_terminal() {
        eprintln!("rich_tui_spike: not a TTY — the plain (rustyline) path handles this. Exiting.");
        return Ok(());
    }
    enable_raw_mode()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(6),
        },
    )?;

    let mut textarea = new_textarea();
    let mut vi = Vi::new();
    let result = run(&mut terminal, &mut textarea, &mut vi);

    disable_raw_mode()?;
    println!();
    result
}

fn new_textarea() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_placeholder_text("type… (Ctrl-D submit · Ctrl-C quit · Esc → vi NORMAL · o/O open line)");
    ta
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    textarea: &mut TextArea<'static>,
    vi: &mut Vi,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| draw(f, textarea, vi))?;
        if !event::poll(Duration::from_millis(250))? {
            continue; // timeout → redraw (live clock)
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match vi.input(key, textarea) {
            Outcome::Quit => return Ok(()),
            Outcome::Submit => {
                submit(terminal, textarea)?;
                *vi = Vi::new();
            }
            Outcome::Continue => {}
        }
    }
}

fn submit(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    textarea: &mut TextArea<'static>,
) -> io::Result<()> {
    let body = textarea.lines().join("\n");
    if body.trim().is_empty() {
        return Ok(());
    }
    let stamp = chrono::Local::now().format("%H:%M:%S").to_string();
    let line_count = textarea.lines().len() as u16;
    terminal.insert_before(line_count + 1, |buf| {
        let mut lines: Vec<Line> = Vec::new();
        for (i, l) in body.lines().enumerate() {
            let prefix = if i == 0 {
                Span::styled(format!("[{stamp}] ❯ "), Style::default().fg(Color::DarkGray))
            } else {
                Span::raw("           ")
            };
            lines.push(Line::from(vec![prefix, Span::raw(l.to_string())]));
        }
        Paragraph::new(lines).render(buf.area, buf);
    })?;
    *textarea = new_textarea();
    Ok(())
}

fn draw(f: &mut Frame, textarea: &TextArea, vi: &Vi) {
    let [status, input] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(f.area());

    let clock = chrono::Local::now().format("%H:%M:%S").to_string();
    let status_line = Line::from(vec![
        Span::styled(
            vi.status(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  spike  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(clock, Style::default().fg(Color::LightBlue)),
    ]);
    f.render_widget(Paragraph::new(status_line), status);
    f.render_widget(textarea, input);
}
