//! Phase-1 spike (issue #416): prove the rich-TUI substrate before porting
//! newt's real input to it. NOT production code — a throwaway example.
//!
//! Proves the hard parts:
//! - **ratatui `Viewport::Inline`** — a pinned bottom region (status row +
//!   multi-line input) with NO alternate screen, so submitted lines scroll into
//!   real terminal scrollback above it (`insert_before`). Copy-paste / SSH survive.
//! - **multi-line vi editing** (tui-textarea) including real `o`/`O`.
//! - a **live light-blue clock** in the status row (timer-driven repaint).
//! - the **non-TTY gate**: piped/headless exits immediately (the plain
//!   rustyline path — the "gills" — would handle that in the real two-mode build).
//!
//! Run on a TTY:  cargo run -p newt-tui --example rich_tui_spike
//! Keys: i/a/A insert · o/O open line · h/j/k/l move · x delete · Esc normal
//!       Ctrl-D submit (scrolls above) · Ctrl-C quit

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

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Normal => "-- NORMAL --",
            Mode::Insert => "-- INSERT --",
        }
    }
}

fn main() -> io::Result<()> {
    // The non-TTY gate (the lungs-vs-gills switch). In the real build this is
    // where run_chat falls back to the rustyline plain path.
    if !io::stdout().is_terminal() {
        eprintln!("rich_tui_spike: not a TTY — the plain (rustyline) path handles this. Exiting.");
        return Ok(());
    }

    enable_raw_mode()?;
    let backend = CrosstermBackend::new(io::stdout());
    // 6 inline rows: 1 status + up to 5 input lines, pinned at the bottom.
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(6),
        },
    )?;

    let mut textarea = new_textarea();
    let mut mode = Mode::Insert;

    let result = run(&mut terminal, &mut textarea, &mut mode);

    disable_raw_mode()?;
    println!(); // leave the cursor below the inline region on exit
    result
}

fn new_textarea() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_placeholder_text("type a message…  (Ctrl-D submit · Ctrl-C quit · Esc for vi normal)");
    ta
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    textarea: &mut TextArea<'static>,
    mode: &mut Mode,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| draw(f, textarea, *mode))?;

        // Poll with a timeout so the clock ticks even when idle.
        if !event::poll(Duration::from_millis(250))? {
            continue; // timeout → redraw (live clock)
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // Global keys (both modes).
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => return Ok(()),
                KeyCode::Char('d') => {
                    submit(terminal, textarea)?;
                    *mode = Mode::Insert;
                    continue;
                }
                _ => {}
            }
        }

        match *mode {
            Mode::Insert => match key.code {
                KeyCode::Esc => *mode = Mode::Normal,
                _ => {
                    textarea.input(key);
                }
            },
            Mode::Normal => match key.code {
                KeyCode::Char('i') => *mode = Mode::Insert,
                KeyCode::Char('a') => {
                    textarea.move_cursor(CursorMove::Forward);
                    *mode = Mode::Insert;
                }
                KeyCode::Char('A') => {
                    textarea.move_cursor(CursorMove::End);
                    *mode = Mode::Insert;
                }
                // The whole point: vi open-line, faithfully (rustyline can't).
                KeyCode::Char('o') => {
                    textarea.move_cursor(CursorMove::End);
                    textarea.insert_newline();
                    *mode = Mode::Insert;
                }
                KeyCode::Char('O') => {
                    textarea.move_cursor(CursorMove::Head);
                    textarea.insert_newline();
                    textarea.move_cursor(CursorMove::Up);
                    *mode = Mode::Insert;
                }
                KeyCode::Char('h') | KeyCode::Left => textarea.move_cursor(CursorMove::Back),
                KeyCode::Char('l') | KeyCode::Right => textarea.move_cursor(CursorMove::Forward),
                KeyCode::Char('j') | KeyCode::Down => textarea.move_cursor(CursorMove::Down),
                KeyCode::Char('k') | KeyCode::Up => textarea.move_cursor(CursorMove::Up),
                KeyCode::Char('0') => textarea.move_cursor(CursorMove::Head),
                KeyCode::Char('$') => textarea.move_cursor(CursorMove::End),
                KeyCode::Char('x') => {
                    textarea.delete_next_char();
                }
                KeyCode::Char('q') => return Ok(()),
                _ => {}
            },
        }
    }
}

/// Submit the buffer: emit it into REAL scrollback above the inline region
/// (proving output scrolls normally), then reset the input.
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

fn draw(f: &mut Frame, textarea: &TextArea, mode: Mode) {
    let [status, input] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(f.area());

    let clock = chrono::Local::now().format("%H:%M:%S").to_string();
    let status_line = Line::from(vec![
        Span::styled(
            mode.label(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  spike  ·  ", Style::default().fg(Color::DarkGray)),
        // The "live" light-blue clock — ticks every ~250ms.
        Span::styled(clock, Style::default().fg(Color::LightBlue)),
    ]);
    f.render_widget(Paragraph::new(status_line), status);
    f.render_widget(textarea, input);
}
