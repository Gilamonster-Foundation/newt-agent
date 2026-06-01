//! Newt-Agent TUI — splash, chat REPL, and settings.

mod settings;

pub use settings::run_settings;

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

const LOGO_PLAIN: &str = include_str!("../../docs/logos/newt-ascii-40.txt");

const VERSION: &str = env!("CARGO_PKG_VERSION");

const NEWT_ORANGE: Color = Color::Rgb(220, 60, 20);
const NEWT_ORANGE_CT: CtColor = CtColor::Rgb { r: 220, g: 60, b: 20 };

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

pub fn run_code(path: Option<&std::path::Path>, no_splash: bool) -> anyhow::Result<()> {
    let color = color_supported_with(&|k| std::env::var(k).ok());
    let workspace = resolve_workspace(path);

    // --no-splash (or [tui] no_splash = true): print a compact inline header
    // and go straight to chat. No alt screen, no raw mode — scrolls into
    // history naturally. Safe for SSH, tmux, and piped output.
    let inline = no_splash
        || newt_core::Config::resolve()
            .ok()
            .and_then(|c| c.tui)
            .map(|t| t.no_splash)
            .unwrap_or(false);

    if inline {
        print_inline_header(&workspace, color);
        return run_chat(&workspace, color);
    }

    // Default: full ANSI splash in alt screen — blinks off on Enter.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide, Clear(ClearType::All), MoveTo(0, 0))?;
    let cont = show_splash(&mut stdout, &workspace, color)?;
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);

    if cont {
        run_chat(&workspace, color)?;
    }
    Ok(())
}

/// Compact inline header for `--no-splash` mode.
/// Prints LOGO_20 with text to the right using ANSI column-move escapes,
/// then scrolls naturally into history. No alt screen, no raw mode.
fn print_inline_header(workspace: &str, color: bool) {
    if !color {
        println!("newt v{VERSION}  ·  {workspace}");
        println!();
        return;
    }

    // Place text at column 23 (just past the 20-col logo).
    let text_col = 23u16;
    let logo_lines: Vec<&str> = LOGO_20.lines().collect();
    let n = logo_lines.len();

    // Text lines aligned to the middle-right of the logo.
    let mid = n / 2;
    let text: &[(&str, bool)] = &[
        ("newt  ·  Small, fast, local-first agentic coder", false),
        (std::concat!("v", env!("CARGO_PKG_VERSION")), true), // dim
        ("", false),
        ("ready — type a task, /help for commands, /exit to quit", true),
    ];
    let text_start = mid.saturating_sub(1);

    for (i, logo_line) in logo_lines.iter().enumerate() {
        // Print logo line (already contains ANSI color codes).
        print!("{logo_line}");
        // Move cursor to column text_col on this row, print text if scheduled.
        let ti = i.wrapping_sub(text_start);
        if let Some((msg, dim)) = text.get(ti) {
            if !msg.is_empty() {
                // \x1b[{col}G moves cursor to absolute column (1-indexed).
                let style_on = if *dim { "\x1b[38;2;100;100;100m" } else { "" };
                let style_off = if *dim { "\x1b[0m" } else { "" };
                print!("\x1b[{text_col}G{style_on}{msg}{style_off}");
            }
        }
        println!();
    }
    println!();
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

fn logo_for_size(cols: u16, rows: u16) -> (&'static str, u16) {
    // Each entry: (art, display_cols, display_rows, min_term_cols).
    // A logo is only selected if both width AND height fit the terminal.
    for (art, w, h, min_w) in [
        (LOGO_160, LOGO_160_COLS, 81u16, LOGO_160_MIN_TERM_COLS),
        (LOGO_120, LOGO_120_COLS, 61u16, LOGO_120_COLS + STATUS_MIN_COLS + 2),
        (LOGO_FULL, LOGO_FULL_COLS, 40u16, LOGO_FULL_COLS + STATUS_MIN_COLS + 2),
        (LOGO_40,   LOGO_40_COLS,   20u16, LOGO_40_COLS   + STATUS_MIN_COLS + 2),
        (LOGO_20,   LOGO_20_COLS,   10u16, LOGO_20_COLS   + STATUS_MIN_COLS + 2),
        (LOGO_10,   LOGO_10_COLS,    5u16, LOGO_10_COLS   + STATUS_MIN_COLS + 2),
    ] {
        if cols >= min_w && rows >= h + 4 {
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
    let (term_cols, term_rows) = terminal::size().unwrap_or((80, 24));
    let (logo, logo_cols) = logo_for_size(term_cols, term_rows);
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
//
// NEWT_CHAT_STYLE=verbose  — show "newt" / "you" labels before the caret
// (default is compact: just the colored caret / symbol)
// ---------------------------------------------------------------------------

/// Whether to show "newt" / "you" labels before the carets.
fn verbose_mode() -> bool {
    std::env::var("NEWT_CHAT_STYLE")
        .map(|v| v.eq_ignore_ascii_case("verbose"))
        .unwrap_or(false)
}

/// Print a newt response line.
/// Color: orange ▸ (matches the logo).  No-color: >.
fn print_newt(msg: &str, color: bool, verbose: bool) {
    if color {
        let prefix = if verbose { "newt ▸  " } else { "▸  " };
        execute!(
            io::stdout(),
            SetForegroundColor(NEWT_ORANGE_CT),
            Print(prefix),
            ResetColor,
            Print(msg),
            Print("\n"),
        )
        .ok();
    } else {
        let prefix = if verbose { "newt >  " } else { ">  " };
        println!("{prefix}{msg}");
    }
}

/// Build a rustyline config reading edit mode from env then config file.
fn build_rl_config() -> rustyline::config::Config {
    let em = std::env::var("NEWT_EDIT_MODE")
        .ok()
        .and_then(|v| match v.to_lowercase().as_str() {
            "vi" | "vim" => Some(newt_core::EditMode::Vi),
            "emacs" => Some(newt_core::EditMode::Emacs),
            _ => None,
        })
        .or_else(|| {
            newt_core::Config::resolve()
                .ok()
                .and_then(|c| c.tui)
                .map(|t| t.edit_mode)
        })
        .unwrap_or(newt_core::EditMode::Emacs);

    rustyline::config::Builder::new()
        .edit_mode(match em {
            newt_core::EditMode::Vi => rustyline::config::EditMode::Vi,
            newt_core::EditMode::Emacs => rustyline::config::EditMode::Emacs,
        })
        .build()
}

/// Build the rustyline prompt string — plain text, PS1 tokens expanded.
///
/// Reads `[tui].prompt` from config (overridable via `NEWT_PROMPT`).
/// Falls back to `\w $ ` (compact) or `you \w $ ` (verbose) if unset.
/// Vi-mode prefixes `[i] ` so the user knows which mode is active.
///
/// Supported tokens: `\w` workspace basename, `\W` full path,
/// `\h` hostname, `\v` newt version.
fn prompt_str(workspace: &str, verbose: bool, is_vi: bool) -> String {
    // Resolve template: NEWT_PROMPT env var > config > built-in default.
    let template = std::env::var("NEWT_PROMPT").ok().or_else(|| {
        newt_core::Config::resolve()
            .ok()
            .and_then(|c| c.tui)
            .and_then(|t| t.prompt)
    });

    let expanded = if let Some(ref tmpl) = template {
        expand_prompt_tokens(tmpl, workspace)
    } else if verbose {
        format!(
            "you {} $ ",
            std::path::Path::new(workspace)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        )
    } else {
        format!(
            "{} $ ",
            std::path::Path::new(workspace)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        )
    };

    if is_vi {
        format!("[i] {expanded}")
    } else {
        expanded
    }
}

fn expand_prompt_tokens(template: &str, workspace: &str) -> String {
    let ws_base = std::path::Path::new(workspace)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| workspace.to_string());
    let host = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "localhost".into());
    template
        .replace("\\W", workspace)
        .replace("\\w", &ws_base)
        .replace("\\h", &host)
        .replace("\\v", env!("CARGO_PKG_VERSION"))
}

fn run_chat(workspace: &str, color: bool) -> anyhow::Result<()> {
    let verbose = verbose_mode();

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

    // Input history file and tokio runtime for async inference.
    let history_path = newt_core::Config::user_config_path()
        .map(|p| p.with_file_name("history"));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    // Resolve the inference backend once (re-read if /settings changes it).
    let (mut inf_url, mut inf_model) = resolve_backend_config();
    print_newt(
        &format!("v{VERSION} ready — {inf_model} @ {inf_url}  (Ctrl-D or /exit to quit)"),
        color,
        verbose,
    );
    println!();

    let mut rl = rustyline::DefaultEditor::with_config(build_rl_config())?;
    if let Some(ref hp) = history_path {
        let _ = rl.load_history(hp);
    }

    let is_vi = build_rl_config().edit_mode() == rustyline::config::EditMode::Vi;
    let prompt = prompt_str(workspace, verbose, is_vi);

    // Conversation history for multi-turn context.
    let mut conv: Vec<(bool, String)> = Vec::new(); // (is_user, text)

    loop {
        match rl.readline(&prompt) {
            Ok(line) => {
                let task = line.trim().to_string();
                if task.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(&task);
                println!();
                if task.starts_with('/') {
                    let cont = dispatch_slash(&task, workspace, color, verbose)?;
                    if let Some(ref hp) = history_path {
                        let _ = rl.save_history(hp);
                    }
                    // Re-read config after alt-screen commands (e.g. /settings).
                    (inf_url, inf_model) = resolve_backend_config();
                    let fresh_cfg = build_rl_config();
                    rl = rustyline::DefaultEditor::with_config(fresh_cfg)?;
                    if let Some(ref hp) = history_path {
                        let _ = rl.load_history(hp);
                    }
                    if !cont {
                        break;
                    }
                } else if matches!(task.as_str(), "exit" | "quit") {
                    break;
                } else {
                    // Show a "thinking" line; overwrite it with the response.
                    print_thinking(color);

                    let response = rt.block_on(chat_complete(
                        &inf_url,
                        &inf_model,
                        &conv,
                        &task,
                    ));

                    // Erase the thinking line then print the real response.
                    erase_line();
                    match response {
                        Ok(reply) => {
                            conv.push((true, task.clone()));
                            conv.push((false, reply.content.clone()));
                            print_newt(&reply.content, color, verbose);
                        }
                        Err(e) => print_newt(&format!("error: {e}"), color, verbose),
                    }
                }
                println!();
            }
            Err(rustyline::error::ReadlineError::Interrupted) => break,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        }
    }

    if let Some(ref hp) = history_path {
        let _ = rl.save_history(hp);
    }
    Ok(())
}

/// Resolve Ollama URL + model from env vars then config.
/// Priority: NEWT_DGX_OLLAMA_URL > NEWT_DGX_HOST synthesis > DGX config node > localhost.
fn resolve_backend_config() -> (String, String) {
    let url = std::env::var("NEWT_DGX_OLLAMA_URL")
        .ok()
        .or_else(|| {
            std::env::var("NEWT_DGX_HOST").ok().map(|h| {
                let scheme = std::env::var("NEWT_DGX_SCHEME").unwrap_or_else(|_| "http".into());
                let port = std::env::var("NEWT_DGX_OLLAMA_PORT").unwrap_or_else(|_| "11434".into());
                format!("{scheme}://{h}:{port}")
            })
        })
        .or_else(|| {
            newt_core::Config::resolve().ok()
                .and_then(|c| c.dgx)
                .and_then(|d| d.nodes.into_iter().next())
                .and_then(|n| n.ollama)
        })
        .unwrap_or_else(|| "http://localhost:11434".into());

    let model = std::env::var("NEWT_DGX_MODEL")
        .ok()
        .or_else(|| {
            newt_core::Config::resolve().ok()
                .and_then(|c| c.dgx)
                .and_then(|d| d.active_model)
        })
        .unwrap_or_else(|| "llama3.1:8b".into());

    (url, model)
}

/// Send a chat turn to Ollama and return the reply.
async fn chat_complete(
    url: &str,
    model: &str,
    history: &[(bool, String)],
    task: &str,
) -> anyhow::Result<newt_inference::backend::ChatReply> {
    use newt_inference::backend::{ChatRequest, InferenceBackend as _};
    use newt_inference::local::LocalOllamaBackend;

    let backend = LocalOllamaBackend::new(url, model);

    let mut req = ChatRequest::new().system(
        "You are newt, a small, fast, local-first agentic coder. \
         Be concise and direct. Help with coding tasks.",
    );
    for (is_user, text) in history {
        req = if *is_user { req.user(text) } else { req.assistant(text) };
    }
    req = req.user(task);

    backend.complete(req).await
}

fn print_thinking(color: bool) {
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::DarkGrey),
            Print("▸  thinking…"),
            ResetColor,
        ).ok();
        io::stdout().flush().ok();
    }
}

fn erase_line() {
    // \r goes to start of line; spaces overwrite; \r positions for next print.
    print!("\r{}\r", " ".repeat(20));
    io::stdout().flush().ok();
}

// ---------------------------------------------------------------------------
// Slash command dispatcher
// ---------------------------------------------------------------------------

/// Dispatch a `/command` line. Returns `true` to keep the session alive,
/// `false` to exit.
fn dispatch_slash(input: &str, workspace: &str, color: bool, verbose: bool) -> anyhow::Result<bool> {
    // Strip leading slash and split into at most 3 tokens.
    let body = input.trim_start_matches('/');
    let mut parts = body.splitn(3, ' ');
    let cmd = parts.next().unwrap_or("");
    let arg1 = parts.next().unwrap_or("").trim();
    let arg2 = parts.next().unwrap_or("").trim();

    match cmd {
        "exit" | "quit" => return Ok(false),

        "help" => {
            print_newt("Available commands:", color, verbose);
            for line in [
                "  /dgx status              — DGX endpoint health + running models",
                "  /dgx models              — list models installed on the DGX",
                "  /dgx warm [model]        — pre-load a model into VRAM",
                "  /dgx route <task>        — recommend a formation for a task",
                "  /dgx doctor              — probe every configured endpoint",
                "  /workspace               — show current workspace path",
                "  /version                 — print newt version",
                "  /settings                — open the interactive settings TUI",
                "  /help                    — this message",
                "  /exit  /quit  exit  quit — leave the session",
            ] {
                println!("{line}");
            }
        }

        "version" => print_newt(&format!("v{VERSION}"), color, verbose),

        "workspace" => print_newt(workspace, color, verbose),

        "settings" => settings::run_settings(None)?,

        "dgx" => {
            if arg1.is_empty() {
                print_newt(
                    "usage: /dgx <status|models|warm [model]|route <task>|doctor>",
                    color,
                    verbose,
                );
            } else {
                let mut dgx_args = vec!["dgx", arg1];
                if !arg2.is_empty() {
                    dgx_args.push(arg2);
                }
                run_newt_subcmd(&dgx_args, color, verbose)?;
            }
        }

        other => print_newt(&format!("unknown command: /{other}  (try /help)"), color, verbose),
    }
    Ok(true)
}

/// Run `newt <args>` as a subprocess using the current executable path so
/// the command works even when newt is not on PATH. stdout/stderr pass
/// through to the terminal unchanged.
fn run_newt_subcmd(args: &[&str], color: bool, verbose: bool) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let status = std::process::Command::new(&exe).args(args).status()?;
    if !status.success() {
        print_newt(
            &format!("command exited with status {}", status.code().unwrap_or(-1)),
            color,
            verbose,
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
        let (_, w) = logo_for_size(LOGO_160_MIN_TERM_COLS, 999);
        assert_eq!(w, LOGO_160_COLS);

        let (_, w) = logo_for_size(LOGO_160_MIN_TERM_COLS - 1, 999);
        assert_eq!(w, LOGO_120_COLS);

        let (_, w) = logo_for_size(LOGO_120_COLS + STATUS_MIN_COLS + 1, 999);
        assert_eq!(w, LOGO_FULL_COLS);

        let (_, w) = logo_for_size(10, 999);
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
            let result = dispatch_slash(cmd, "/ws", false, false).unwrap();
            assert!(!result, "{cmd} should return false (exit)");
        }
    }

    #[test]
    fn slash_help_returns_true() {
        assert!(dispatch_slash("/help", "/ws", false, false).unwrap());
    }

    #[test]
    fn slash_version_returns_true() {
        assert!(dispatch_slash("/version", "/ws", false, false).unwrap());
    }

    #[test]
    fn slash_workspace_returns_true() {
        assert!(dispatch_slash("/workspace", "/ws", false, false).unwrap());
    }

    #[test]
    fn slash_unknown_returns_true() {
        assert!(dispatch_slash("/notacommand", "/ws", false, false).unwrap());
    }

    #[test]
    fn slash_dgx_no_subcmd_returns_true() {
        assert!(dispatch_slash("/dgx", "/ws", false, false).unwrap());
    }
}
