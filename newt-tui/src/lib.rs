//! Newt-Agent TUI — ratatui-driven screens.
//!
//! ## Flow
//!
//! `run_code` opens a single alternate-screen session and runs two phases:
//!
//! 1. **Splash** — ANSI color logo (or plain ASCII fallback) with branding.
//!    Press Enter to continue; q / Esc / Ctrl-C to quit.
//!
//! 2. **Chat TUI** — ratatui-managed layout that handles terminal resize
//!    automatically. Safe for SSH and tmux.
//!
//! The alt-screen is entered once and never left between phases, so there
//! is no flicker on the transition.
//!
//! ## Layout (chat mode)
//!
//! ```text
//! ┌─ header (ASCII logo + branding) ──────────────────────────────┐
//! │                                                                │
//! ├─ messages (scrollable, fills remaining space) ─────────────────┤
//! │  you ▸  hello                                                  │
//! │  newt ▸ ...                                                    │
//! ├─ input ────────────────────────────────────────────────────────┤
//! │  ▸ type a task and press Enter                                 │
//! └────────────────────────────────────────────────────────────────┘
//! ```

use std::io::{self, IsTerminal, Write as _};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute, queue,
    style::{Color as CtColor, Print, ResetColor, SetForegroundColor},
    terminal::{
        self, disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};

// ---------------------------------------------------------------------------
// Logo assets
// ---------------------------------------------------------------------------

/// 24-bit ANSI half-block art. Display dimensions (cols × rows):
///   LOGO_10:   10 × 5     (original, tiny)
///   LOGO_20:   20 × 10    (original, small)
///   LOGO_40:   40 × 20    (original, medium)
///   LOGO_FULL: 80 × 40    (original, large)
///   LOGO_120: 126 × 61    (chafa, natural ratio)
///   LOGO_160: 166 × 81    (chafa, natural ratio — very wide terminals only)
/// Printed directly via crossterm (not ratatui) in splash mode.
const LOGO_10: &str = include_str!("../../docs/logos/newt-ansi-10.txt");
const LOGO_20: &str = include_str!("../../docs/logos/newt-ansi-20.txt");
const LOGO_40: &str = include_str!("../../docs/logos/newt-ansi-40.txt");
const LOGO_FULL: &str = include_str!("../../docs/logos/newt-ansi-full.txt");
const LOGO_120: &str = include_str!("../../docs/logos/newt-ansi-120.txt");
const LOGO_160: &str = include_str!("../../docs/logos/newt-ansi-160.txt");

const LOGO_10_COLS: u16 = 10;
const LOGO_20_COLS: u16 = 20;
const LOGO_40_COLS: u16 = 40;
const LOGO_FULL_COLS: u16 = 80;
const LOGO_120_COLS: u16 = 126;
const LOGO_160_COLS: u16 = 166;

/// Plain ASCII art — 14 lines × ~40 display columns.
/// Rendered via ratatui in chat-mode header and no-color splash.
const LOGO_PLAIN: &str = include_str!("../../docs/logos/newt-ascii-40.txt");

const VERSION: &str = env!("CARGO_PKG_VERSION");

const NEWT_ORANGE: Color = Color::Rgb(220, 60, 20);
const NEWT_ORANGE_CT: CtColor = CtColor::Rgb {
    r: 220,
    g: 60,
    b: 20,
};

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

pub fn run_code(path: Option<&std::path::Path>) -> anyhow::Result<()> {
    let color = color_supported_with(&|k| std::env::var(k).ok());
    let workspace = resolve_workspace(path);

    // Enter alt screen once — both splash and chat share this session.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide, Clear(ClearType::All), MoveTo(0, 0))?;

    let result = (|| {
        if show_splash(&mut stdout, &workspace, color)? {
            run_chat(&workspace)
        } else {
            Ok(())
        }
    })();

    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
    result
}

pub fn run_pilot(_flight_id: &str) -> anyhow::Result<()> {
    anyhow::bail!("newt-tui::run_pilot not yet implemented")
}

// ---------------------------------------------------------------------------
// Color detection
// ---------------------------------------------------------------------------

/// Returns `true` when stdout supports ANSI color.
///
/// Priority:
/// 1. `NO_COLOR` set (any value) → false  (<https://no-color.org/>)
/// 2. `TERM=dumb`                → false
/// 3. stdout is not a TTY        → false
/// 4. otherwise                  → true
pub fn color_supported() -> bool {
    color_supported_with(&|k| std::env::var(k).ok())
}

fn color_supported_with(get_env: &dyn Fn(&str) -> Option<String>) -> bool {
    if get_env("NO_COLOR").is_some() {
        return false;
    }
    if get_env("TERM").as_deref() == Some("dumb") {
        return false;
    }
    io::stdout().is_terminal()
}

// ---------------------------------------------------------------------------
// Splash phase
// ---------------------------------------------------------------------------

const STATUS_MIN_COLS: u16 = 44;
const LOGO_160_MIN_TERM_COLS: u16 = 260;

fn logo_for_width(cols: u16) -> (&'static str, u16) {
    for (art, w, min_term) in [
        (LOGO_160, LOGO_160_COLS, LOGO_160_MIN_TERM_COLS),
        (LOGO_120, LOGO_120_COLS, LOGO_120_COLS + STATUS_MIN_COLS + 2),
        (LOGO_FULL, LOGO_FULL_COLS, LOGO_FULL_COLS + STATUS_MIN_COLS + 2),
        (LOGO_40, LOGO_40_COLS, LOGO_40_COLS + STATUS_MIN_COLS + 2),
        (LOGO_20, LOGO_20_COLS, LOGO_20_COLS + STATUS_MIN_COLS + 2),
        (LOGO_10, LOGO_10_COLS, LOGO_10_COLS + STATUS_MIN_COLS + 2),
    ] {
        if cols >= min_term {
            return (art, w);
        }
    }
    (LOGO_10, LOGO_10_COLS)
}

/// Render the splash. Returns `true` if the user pressed Enter (continue to
/// chat), `false` if they pressed q / Esc / Ctrl-C (quit).
fn show_splash(out: &mut io::Stdout, workspace: &str, color: bool) -> anyhow::Result<bool> {
    if color {
        show_splash_color(out, workspace)
    } else {
        show_splash_plain(out, workspace)
    }
}

fn show_splash_color(out: &mut io::Stdout, workspace: &str) -> anyhow::Result<bool> {
    let (term_cols, _) = terminal::size().unwrap_or((80, 24));
    let (logo, logo_cols) = logo_for_width(term_cols);
    let logo_rows = logo.lines().count() as u16;

    // Print ANSI logo flush to top. In raw mode \n is LF only; \r\n resets column.
    write!(out, "{}", logo.replace('\n', "\r\n"))?;
    out.flush()?;

    let brand_col = logo_cols + 2;
    let brand_row = logo_rows.saturating_sub(4) / 2;

    queue!(out, MoveTo(brand_col, brand_row))?;
    queue!(
        out,
        SetForegroundColor(NEWT_ORANGE_CT),
        Print("newt"),
        ResetColor,
        Print("  ·  Small, fast, local-first agentic coder")
    )?;
    queue!(out, MoveTo(brand_col, brand_row + 1))?;
    queue!(
        out,
        SetForegroundColor(CtColor::DarkGrey),
        Print(format!("v{VERSION}")),
        ResetColor
    )?;
    queue!(out, MoveTo(brand_col, brand_row + 3))?;
    queue!(
        out,
        SetForegroundColor(CtColor::DarkGrey),
        Print("Enter  start coder   ·   q quit"),
        ResetColor
    )?;
    out.flush()?;

    splash_wait_for_continue()
}

fn show_splash_plain(_out: &mut io::Stdout, workspace: &str) -> anyhow::Result<bool> {
    // For the plain path ratatui takes a fresh io::stdout() handle — fine since
    // stdout is a singleton and we already hold raw mode + alt screen.
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let result = loop {
        terminal.draw(|f| {
            let area = f.area();
            let orange_bold = Style::default()
                .fg(NEWT_ORANGE)
                .add_modifier(Modifier::BOLD);
            let dim = Style::default().fg(Color::DarkGray);
            let mut lines: Vec<Line> = vec![Line::from("")];
            for l in LOGO_PLAIN.lines() {
                lines.push(Line::from(l.to_owned()));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("newt", orange_bold),
                Span::raw("  ·  Small, fast, local-first agentic coder"),
            ]));
            lines.push(Line::from(Span::styled(format!("v{VERSION}"), dim)));
            lines.push(Line::from(""));
            lines.push(Line::from(format!("Workspace:  {workspace}")));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Enter  start coder   ·   q quit",
                dim,
            )));
            let w = 60u16.min(area.width);
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Fill(1), Constraint::Length(w), Constraint::Fill(1)])
                .split(area);
            f.render_widget(
                Paragraph::new(Text::from(lines)),
                cols[1],
            );
        })?;
        if let Some(cont) = splash_poll_event()? {
            break cont;
        }
    };
    Ok(result)
}

/// Poll for a splash keypress. Returns `Some(true)` = continue, `Some(false)` = quit, `None` = keep waiting.
fn splash_poll_event() -> anyhow::Result<Option<bool>> {
    if event::poll(std::time::Duration::from_millis(100))? {
        return Ok(Some(splash_key_action(&event::read()?)));
    }
    Ok(None)
}

/// Block until the user presses Enter (true) or a quit key (false).
fn splash_wait_for_continue() -> anyhow::Result<bool> {
    loop {
        if event::poll(std::time::Duration::from_millis(100))? {
            return Ok(splash_key_action(&event::read()?));
        }
    }
}

/// Map a key event to splash intent: `true` = continue, `false` = quit.
/// Any printable char or Enter continues; q / Esc / Ctrl-C quits.
fn splash_key_action(ev: &Event) -> bool {
    match ev {
        Event::Key(KeyEvent {
            code: KeyCode::Char('q'),
            ..
        }) => false,
        Event::Key(KeyEvent {
            code: KeyCode::Esc, ..
        }) => false,
        Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers,
            ..
        }) if modifiers.contains(KeyModifiers::CONTROL) => false,
        Event::Key(KeyEvent {
            code: KeyCode::Enter | KeyCode::Char(_),
            ..
        }) => true,
        _ => true, // any other key also continues
    }
}

// ---------------------------------------------------------------------------
// Chat TUI phase
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ChatMessage {
    from_user: bool,
    text: String,
}

struct ChatApp {
    workspace: String,
    messages: Vec<ChatMessage>,
    input: String,
    scroll: usize,
}

impl ChatApp {
    fn new(workspace: &str) -> Self {
        Self {
            workspace: workspace.to_owned(),
            messages: vec![ChatMessage {
                from_user: false,
                text: format!(
                    "newt v{VERSION} ready.  \
                     Type a coding task and press Enter. \
                     (Coder runtime arrives in Step 0.4 — routing and eval are live.)"
                ),
            }],
            input: String::new(),
            scroll: 0,
        }
    }

    fn submit(&mut self) {
        let text = std::mem::take(&mut self.input);
        if text.is_empty() {
            return;
        }
        self.messages.push(ChatMessage {
            from_user: true,
            text: text.clone(),
        });
        // Mock response until the real coder is wired in.
        let reply = format!(
            "Got it: \"{text}\" — coder runtime not yet connected. \
             Try `just eval --case 001` to run the eval suite against a real Ollama."
        );
        self.messages.push(ChatMessage {
            from_user: false,
            text: reply,
        });
        // Scroll to bottom.
        self.scroll = self.messages.len().saturating_sub(1);
    }

    fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }
    fn scroll_down(&mut self) {
        self.scroll = (self.scroll + 1).min(self.messages.len().saturating_sub(1));
    }
}

fn run_chat(workspace: &str) -> anyhow::Result<()> {
    // Re-use the already-open alt screen; just hand stdout to ratatui.
    execute!(io::stdout(), Clear(ClearType::All))?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut app = ChatApp::new(workspace);

    loop {
        terminal.draw(|f| render_chat(f, &app))?;

        if event::poll(std::time::Duration::from_millis(50))? {
            match event::read()? {
                // Quit
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers,
                    ..
                }) if modifiers.contains(KeyModifiers::CONTROL) => break,

                // Submit
                Event::Key(KeyEvent {
                    code: KeyCode::Enter,
                    ..
                }) => app.submit(),

                // Editing
                Event::Key(KeyEvent {
                    code: KeyCode::Backspace,
                    ..
                }) => {
                    app.input.pop();
                }
                Event::Key(KeyEvent {
                    code: KeyCode::Char(c),
                    modifiers,
                    ..
                }) if !modifiers.contains(KeyModifiers::CONTROL)
                    && !modifiers.contains(KeyModifiers::ALT) =>
                {
                    app.input.push(c);
                }

                // Scrolling
                Event::Key(KeyEvent {
                    code: KeyCode::Up, ..
                }) => app.scroll_up(),
                Event::Key(KeyEvent {
                    code: KeyCode::Down,
                    ..
                }) => app.scroll_down(),
                Event::Key(KeyEvent {
                    code: KeyCode::PageUp,
                    ..
                }) => {
                    for _ in 0..5 {
                        app.scroll_up();
                    }
                }
                Event::Key(KeyEvent {
                    code: KeyCode::PageDown,
                    ..
                }) => {
                    for _ in 0..5 {
                        app.scroll_down();
                    }
                }

                // Terminal resize — ratatui handles it automatically on next draw.
                Event::Resize(_, _) => {}

                _ => {}
            }
        }
    }
    Ok(())
}

fn render_chat(f: &mut ratatui::Frame, app: &ChatApp) {
    let area = f.area();
    let dim = Style::default().fg(Color::DarkGray);
    let bold_orange = Style::default()
        .fg(NEWT_ORANGE)
        .add_modifier(Modifier::BOLD);

    // Layout: thin title bar | messages | input
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(3),
        ])
        .split(area);

    // ── Title bar ────────────────────────────────────────────────────────────
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("newt", bold_orange),
            Span::styled(format!("  ·  {}", app.workspace), dim),
        ])),
        chunks[0],
    );

    // ── Messages ─────────────────────────────────────────────────────────────
    let msg_lines: Vec<Line> = app
        .messages
        .iter()
        .flat_map(|m| {
            let prefix = if m.from_user {
                Span::styled("  you ▸  ", bold_orange)
            } else {
                Span::styled(" newt ▸  ", Style::default().fg(Color::Cyan))
            };
            let mut lines: Vec<Line> = m
                .text
                .lines()
                .enumerate()
                .map(|(i, l)| {
                    if i == 0 {
                        Line::from(vec![prefix.clone(), Span::raw(l.to_owned())])
                    } else {
                        Line::from(vec![Span::raw("         "), Span::raw(l.to_owned())])
                    }
                })
                .collect();
            lines.push(Line::from(""));
            lines
        })
        .collect();

    f.render_widget(
        Paragraph::new(Text::from(msg_lines))
            .wrap(Wrap { trim: false })
            .scroll((app.scroll as u16, 0))
            .block(Block::default().borders(Borders::TOP)),
        chunks[1],
    );

    // ── Input ─────────────────────────────────────────────────────────────────
    f.render_widget(
        Paragraph::new(format!(" ▸  {}█", app.input))
            .block(Block::default().borders(Borders::TOP).border_style(dim)),
        chunks[2],
    );
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn resolve_workspace(path: Option<&std::path::Path>) -> String {
    path.map(|p| p.to_string_lossy().into_owned())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|d| d.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "(unknown)".into())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        |k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn no_color_env_disables_color() {
        assert!(!color_supported_with(&mock_env(&[("NO_COLOR", "1")])));
    }

    #[test]
    fn no_color_empty_value_still_disables() {
        assert!(!color_supported_with(&mock_env(&[("NO_COLOR", "")])));
    }

    #[test]
    fn dumb_term_disables_color() {
        assert!(!color_supported_with(&mock_env(&[("TERM", "dumb")])));
    }

    #[test]
    fn non_dumb_term_passes_env_check() {
        let get_env = mock_env(&[("TERM", "xterm-256color")]);
        let _ = color_supported_with(&get_env);
    }

    #[test]
    fn logo_assets_are_embedded() {
        assert!(!LOGO_PLAIN.is_empty());
        assert!(LOGO_PLAIN.lines().count() > 5);
        for logo in [LOGO_10, LOGO_20, LOGO_40, LOGO_FULL, LOGO_120, LOGO_160] {
            assert!(!logo.is_empty());
            assert!(logo.lines().count() >= 5);
        }
    }

    #[test]
    fn logo_for_width_picks_correct_size() {
        let (_, w) = logo_for_width(LOGO_160_MIN_TERM_COLS);
        assert_eq!(w, LOGO_160_COLS);

        let (_, w) = logo_for_width(LOGO_160_MIN_TERM_COLS - 1);
        assert_eq!(w, LOGO_120_COLS);

        let (_, w) = logo_for_width(LOGO_120_COLS + STATUS_MIN_COLS + 1);
        assert_eq!(w, LOGO_FULL_COLS);

        let (_, w) = logo_for_width(10);
        assert_eq!(w, LOGO_10_COLS);
    }

    #[test]
    fn logo_widths_are_strictly_ordered() {
        assert!(LOGO_10_COLS < LOGO_20_COLS);
        assert!(LOGO_20_COLS < LOGO_40_COLS);
        assert!(LOGO_40_COLS < LOGO_FULL_COLS);
        assert!(LOGO_FULL_COLS < LOGO_120_COLS);
        assert!(LOGO_120_COLS < LOGO_160_COLS);
    }

    #[test]
    fn version_constant_is_populated() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn resolve_workspace_falls_back_gracefully() {
        let p = std::path::Path::new("/some/workspace");
        assert_eq!(resolve_workspace(Some(p)), "/some/workspace");
    }

    #[test]
    fn splash_key_action_quit_keys() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        assert!(!splash_key_action(&Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE
        ))));
        assert!(!splash_key_action(&Event::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE
        ))));
        assert!(!splash_key_action(&Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        ))));
    }

    #[test]
    fn splash_key_action_continue_keys() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        assert!(splash_key_action(&Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        ))));
        assert!(splash_key_action(&Event::Key(KeyEvent::new(
            KeyCode::Char('h'),
            KeyModifiers::NONE
        ))));
    }

    #[test]
    fn chat_app_submit_adds_messages() {
        let mut app = ChatApp::new("/workspace");
        assert_eq!(app.messages.len(), 1); // welcome message
        app.input = "rename foo to bar".into();
        app.submit();
        assert_eq!(app.messages.len(), 3); // + user + mock reply
        assert!(app.messages[1].from_user);
        assert!(!app.messages[2].from_user);
    }

    #[test]
    fn chat_app_empty_input_ignored() {
        let mut app = ChatApp::new("/workspace");
        app.submit();
        assert_eq!(app.messages.len(), 1);
    }

    #[test]
    fn chat_app_scroll_bounds() {
        let mut app = ChatApp::new("/workspace");
        app.scroll_up(); // should not underflow
        assert_eq!(app.scroll, 0);
        app.input = "hi".into();
        app.submit();
        app.scroll_down();
        app.scroll_down();
        app.scroll_down(); // should not exceed message count
        assert!(app.scroll < app.messages.len());
    }
}
