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
    widgets::Paragraph,
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

    // Splash runs in alt screen + raw mode.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide, Clear(ClearType::All), MoveTo(0, 0))?;
    let cont = show_splash(&mut stdout, &workspace, color)?;

    // Return to the normal terminal before the chat loop so the user's
    // own scrollback handles history — no custom scroll management needed.
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);

    if cont {
        run_chat(&workspace, color)?;
    }
    Ok(())
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

fn show_splash_color(out: &mut io::Stdout, _workspace: &str) -> anyhow::Result<bool> {
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
// Chat — plain terminal REPL
//
// No alternate screen, no custom scroll. The terminal's own scrollback
// buffer handles history. Works identically over SSH and inside tmux.
// ---------------------------------------------------------------------------

fn print_newt(msg: &str, color: bool) {
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::Cyan),
            Print(" newt ▸  "),
            ResetColor,
            Print(msg),
            Print("\n"),
        )
        .ok();
    } else {
        println!(" newt >  {msg}");
    }
}

fn run_chat(workspace: &str, color: bool) -> anyhow::Result<()> {
    use std::io::BufRead as _;

    // Header line — one-time print, then normal scroll from here.
    if color {
        execute!(
            io::stdout(),
            Print("\n"),
            SetForegroundColor(NEWT_ORANGE_CT),
            Print("newt"),
            ResetColor,
            SetForegroundColor(CtColor::DarkGrey),
            Print(format!("  ·  {workspace}\n")),
            ResetColor,
        )?;
    } else {
        println!("\nnewt  ·  {workspace}");
    }

    print_newt(
        &format!(
            "v{VERSION} ready. Type a task and press Enter. (Ctrl-D or Ctrl-C to quit.)"
        ),
        color,
    );
    println!();

    let stdin = io::stdin();
    let mut line = String::new();

    loop {
        // Prompt: label in default color, caret in orange as a visual accent.
        if color {
            execute!(
                io::stdout(),
                Print("  you "),
                SetForegroundColor(NEWT_ORANGE_CT),
                Print("▸"),
                ResetColor,
                Print("  "),
            )?;
        } else {
            print!("  you >  ");
        }
        io::stdout().flush()?;

        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF / Ctrl-D
            Ok(_) => {
                let task = line.trim();
                if task.is_empty() {
                    continue;
                }
                println!();
                if task.starts_with('/') {
                    if !dispatch_slash(task, workspace, color)? {
                        break;
                    }
                } else if matches!(task, "exit" | "quit") {
                    break;
                } else {
                    print_newt(
                        &format!(
                            "Got it: \"{task}\" — coder runtime not yet connected. \
                             Try `just eval --case 001` to run against a real Ollama."
                        ),
                        color,
                    );
                }
                println!();
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Slash command dispatcher
// ---------------------------------------------------------------------------

/// Dispatch a `/command` line. Returns `true` to keep the session alive,
/// `false` to exit.
fn dispatch_slash(input: &str, workspace: &str, color: bool) -> anyhow::Result<bool> {
    // Strip leading slash and split into at most 3 tokens.
    let body = input.trim_start_matches('/');
    let mut parts = body.splitn(3, ' ');
    let cmd = parts.next().unwrap_or("");
    let arg1 = parts.next().unwrap_or("").trim();
    let arg2 = parts.next().unwrap_or("").trim();

    match cmd {
        "exit" | "quit" => return Ok(false),

        "help" => {
            print_newt("Available commands:", color);
            for line in [
                "  /dgx status              — DGX endpoint health + running models",
                "  /dgx models              — list models installed on the DGX",
                "  /dgx warm [model]        — pre-load a model into VRAM",
                "  /dgx route <task>        — recommend a formation for a task",
                "  /dgx doctor              — probe every configured endpoint",
                "  /workspace               — show current workspace path",
                "  /version                 — print newt version",
                "  /help                    — this message",
                "  /exit  /quit  exit  quit — leave the session",
            ] {
                println!("{line}");
            }
        }

        "version" => print_newt(&format!("v{VERSION}"), color),

        "workspace" => print_newt(workspace, color),

        "dgx" => {
            if arg1.is_empty() {
                print_newt(
                    "usage: /dgx <status|models|warm [model]|route <task>|doctor>",
                    color,
                );
            } else {
                // Build args: ["dgx", subcmd, optional-extra]
                let mut dgx_args = vec!["dgx", arg1];
                if !arg2.is_empty() {
                    dgx_args.push(arg2);
                }
                run_newt_subcmd(&dgx_args, color)?;
            }
        }

        other => print_newt(&format!("unknown command: /{other}  (try /help)"), color),
    }
    Ok(true)
}

/// Run `newt <args>` as a subprocess using the current executable path so
/// the command works even when newt is not on PATH. stdout/stderr pass
/// through to the terminal unchanged.
fn run_newt_subcmd(args: &[&str], color: bool) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let status = std::process::Command::new(&exe).args(args).status()?;
    if !status.success() {
        print_newt(
            &format!(
                "command exited with status {}",
                status.code().unwrap_or(-1)
            ),
            color,
        );
    }
    Ok(())
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
    fn slash_exit_returns_false() {
        for cmd in ["/exit", "/quit"] {
            let result = dispatch_slash(cmd, "/ws", false).unwrap();
            assert!(!result, "{cmd} should return false (exit)");
        }
    }

    #[test]
    fn slash_help_returns_true() {
        assert!(dispatch_slash("/help", "/ws", false).unwrap());
    }

    #[test]
    fn slash_version_returns_true() {
        assert!(dispatch_slash("/version", "/ws", false).unwrap());
    }

    #[test]
    fn slash_workspace_returns_true() {
        assert!(dispatch_slash("/workspace", "/ws", false).unwrap());
    }

    #[test]
    fn slash_unknown_returns_true() {
        assert!(dispatch_slash("/notacommand", "/ws", false).unwrap());
    }

    #[test]
    fn slash_dgx_no_subcmd_returns_true() {
        assert!(dispatch_slash("/dgx", "/ws", false).unwrap());
    }
}
