//! Newt-Agent TUI — ratatui-driven screens.
//!
//! - `code` mode: splash / start screen (Step 0.2); full chat pane (Step 0.4+).
//! - `pilot` mode: drake-swarm dashboard (later step — stub).
//!
//! ## Color path vs plain path
//!
//! The color logo files (`newt-ansi-*.txt`) use raw 24-bit ANSI escape codes
//! that ratatui cannot render through its widget system. On color-capable
//! terminals we print the logo directly with crossterm and use crossterm for
//! cursor positioning and the keyboard event loop. On plain/dumb terminals we
//! fall back to ratatui + the ASCII-only logo, which renders correctly
//! everywhere.

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
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};

/// 24-bit ANSI half-block art. Display dimensions (cols × rows):
///   LOGO_10:   10 × 5
///   LOGO_20:   20 × 10
///   LOGO_40:   40 × 20
///   LOGO_FULL: 80 × 40
/// Chosen at runtime by `logo_for_width`. Printed directly (not ratatui).
const LOGO_10: &str = include_str!("../../docs/logos/newt-ansi-10.txt");
const LOGO_20: &str = include_str!("../../docs/logos/newt-ansi-20.txt");
const LOGO_40: &str = include_str!("../../docs/logos/newt-ansi-40.txt");
const LOGO_FULL: &str = include_str!("../../docs/logos/newt-ansi-full.txt");

/// Display column widths matching the four logo constants.
const LOGO_10_COLS: u16 = 10;
const LOGO_20_COLS: u16 = 20;
const LOGO_40_COLS: u16 = 40;
const LOGO_FULL_COLS: u16 = 80;

/// Plain ASCII art — 14 lines × ~40 display columns.
/// Used as the no-color fallback, rendered as a ratatui Paragraph.
const LOGO_PLAIN: &str = include_str!("../../docs/logos/newt-ascii-40.txt");

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Newt's brand orange, sampled from the ANSI logo palette.
const NEWT_ORANGE_CT: CtColor = CtColor::Rgb {
    r: 220,
    g: 60,
    b: 20,
};

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

pub fn run_code(path: Option<&std::path::Path>) -> anyhow::Result<()> {
    if color_supported_with(&|k| std::env::var(k).ok()) {
        run_splash_color(path)
    } else {
        run_splash_plain(path)
    }
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
// Color splash — crossterm direct rendering
// ---------------------------------------------------------------------------

/// Pick the largest ANSI logo whose display width fits within `cols`,
/// leaving at least `STATUS_MIN_COLS` columns for the status panel.
/// Returns `(art, display_col_width)`.
///
/// Status panel minimum — enough for "Workspace: /very/long/path".
const STATUS_MIN_COLS: u16 = 44;

fn logo_for_width(cols: u16) -> (&'static str, u16) {
    for (art, w) in [
        (LOGO_FULL, LOGO_FULL_COLS),
        (LOGO_40, LOGO_40_COLS),
        (LOGO_20, LOGO_20_COLS),
        (LOGO_10, LOGO_10_COLS),
    ] {
        if w + STATUS_MIN_COLS + 2 <= cols {
            return (art, w);
        }
    }
    (LOGO_10, LOGO_10_COLS)
}

fn run_splash_color(path: Option<&std::path::Path>) -> anyhow::Result<()> {
    let workspace = resolve_workspace(path);
    let term_cols = terminal::size().map(|(w, _)| w).unwrap_or(80);
    let (logo, logo_cols) = logo_for_width(term_cols);
    let logo_rows = logo.lines().count() as u16;

    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(
        out,
        EnterAlternateScreen,
        Hide,
        Clear(ClearType::All),
        MoveTo(0, 0)
    )?;

    // Print the ANSI logo directly — the file already contains all escape codes.
    // In raw mode \n is line-feed only; replace with \r\n so each new line
    // starts at column 0 (carriage return is not implicit in raw mode).
    write!(out, "{}", logo.replace('\n', "\r\n"))?;
    out.flush()?;

    // Status panel: to the right of the logo, vertically centred.
    let status_col = logo_cols + 2;
    let start_row = logo_rows.saturating_sub(6) / 2;

    let status: &[&dyn Fn(&mut io::Stdout) -> anyhow::Result<()>] = &[
        &|o| {
            queue!(
                o,
                SetForegroundColor(NEWT_ORANGE_CT),
                Print("newt"),
                ResetColor,
                Print("  ·  Small, fast, local-first agentic coder")
            )?;
            Ok(())
        },
        &|o| {
            queue!(
                o,
                SetForegroundColor(CtColor::DarkGrey),
                Print(format!("v{VERSION}")),
                ResetColor
            )?;
            Ok(())
        },
        &|_| Ok(()),
        &|o| {
            queue!(o, Print(format!("Workspace:  {workspace}")))?;
            Ok(())
        },
        &|_| Ok(()),
        &|o| {
            queue!(
                o,
                SetForegroundColor(CtColor::DarkGrey),
                Print("q quit  ·  Ctrl-C quit  ·  coder UI coming soon"),
                ResetColor
            )?;
            Ok(())
        },
    ];

    for (i, render) in status.iter().enumerate() {
        queue!(out, MoveTo(status_col, start_row + i as u16))?;
        render(&mut out)?;
    }
    out.flush()?;

    splash_event_loop()?;

    let _ = disable_raw_mode();
    let _ = execute!(out, Show, LeaveAlternateScreen);
    Ok(())
}

// ---------------------------------------------------------------------------
// Plain splash — ratatui rendering
// ---------------------------------------------------------------------------

fn run_splash_plain(path: Option<&std::path::Path>) -> anyhow::Result<()> {
    let workspace = resolve_workspace(path);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = plain_render_loop(&mut terminal, &workspace);

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    result
}

fn plain_render_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    workspace: &str,
) -> anyhow::Result<()> {
    loop {
        terminal.draw(|f| render_plain_splash(f, workspace))?;
        if splash_event_poll()? {
            break;
        }
    }
    Ok(())
}

fn render_plain_splash(f: &mut ratatui::Frame, workspace: &str) {
    let area = f.area();

    let logo_style = Style::default();
    let accent = Style::default()
        .fg(Color::Rgb(220, 60, 20))
        .add_modifier(Modifier::BOLD);
    let dim = Style::default();

    let mut lines: Vec<Line> = vec![Line::from("")];
    for l in LOGO_PLAIN.lines() {
        lines.push(Line::from(Span::styled(l.to_owned(), logo_style)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("newt", accent),
        Span::raw("  ·  Small, fast, local-first agentic coder"),
    ]));
    lines.push(Line::from(format!("v{VERSION}")));
    lines.push(Line::from(""));
    lines.push(Line::from(format!("Workspace:  {workspace}")));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "q quit  ·  Ctrl-C quit  ·  full coder UI coming soon",
        dim,
    )));

    let content_width = 60u16.min(area.width);
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(content_width),
            Constraint::Fill(1),
        ])
        .split(area);

    f.render_widget(
        Paragraph::new(Text::from(lines))
            .alignment(Alignment::Left)
            .block(Block::default().borders(Borders::NONE)),
        chunks[1],
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

/// Poll once for a quit key. Returns `true` if the user requested exit.
fn splash_event_poll() -> anyhow::Result<bool> {
    if event::poll(std::time::Duration::from_millis(100))? {
        return Ok(matches_quit(&event::read()?));
    }
    Ok(false)
}

/// Block until the user presses a quit key.
fn splash_event_loop() -> anyhow::Result<()> {
    loop {
        if event::poll(std::time::Duration::from_millis(100))? && matches_quit(&event::read()?) {
            break;
        }
    }
    Ok(())
}

fn matches_quit(ev: &Event) -> bool {
    matches!(
        ev,
        Event::Key(KeyEvent {
            code: KeyCode::Char('q'),
            ..
        }) | Event::Key(KeyEvent {
            code: KeyCode::Esc,
            ..
        })
    ) || matches!(ev, Event::Key(KeyEvent {
        code: KeyCode::Char('c'),
        modifiers,
        ..
    }) if modifiers.contains(KeyModifiers::CONTROL))
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
        // is_terminal() depends on the test harness; we only verify the env
        // checks don't block a real terminal name.
        let get_env = mock_env(&[("TERM", "xterm-256color")]);
        let _ = color_supported_with(&get_env); // must not panic
    }

    #[test]
    fn logo_assets_are_embedded() {
        assert!(!LOGO_PLAIN.is_empty());
        assert!(LOGO_PLAIN.lines().count() > 5);
        for logo in [LOGO_10, LOGO_20, LOGO_40, LOGO_FULL] {
            assert!(!logo.is_empty());
            assert!(logo.lines().count() >= 5);
        }
    }

    #[test]
    fn logo_for_width_picks_correct_size() {
        // LOGO_FULL (80 cols) needs 80 + STATUS_MIN_COLS + 2 = 126 cols.
        let (_, w) = logo_for_width(200);
        assert_eq!(w, LOGO_FULL_COLS);

        let (_, w) = logo_for_width(LOGO_FULL_COLS + STATUS_MIN_COLS + 2);
        assert_eq!(w, LOGO_FULL_COLS);

        // Just below the LOGO_FULL threshold → should pick LOGO_40.
        let (_, w) = logo_for_width(LOGO_FULL_COLS + STATUS_MIN_COLS + 1);
        assert_eq!(w, LOGO_40_COLS);

        // Narrow terminal falls back to smallest.
        let (_, w) = logo_for_width(10);
        assert_eq!(w, LOGO_10_COLS);
    }

    #[test]
    fn logo_widths_are_strictly_ordered() {
        assert!(LOGO_10_COLS < LOGO_20_COLS);
        assert!(LOGO_20_COLS < LOGO_40_COLS);
        assert!(LOGO_40_COLS < LOGO_FULL_COLS);
    }

    #[test]
    fn version_constant_is_populated() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn resolve_workspace_falls_back_gracefully() {
        // With an explicit path the value is returned verbatim.
        let p = std::path::Path::new("/some/workspace");
        assert_eq!(resolve_workspace(Some(p)), "/some/workspace");
    }

    #[test]
    fn matches_quit_recognises_q_and_esc() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let q = Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        let esc = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let other = Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(matches_quit(&q));
        assert!(matches_quit(&esc));
        assert!(matches_quit(&ctrl_c));
        assert!(!matches_quit(&other));
    }
}
