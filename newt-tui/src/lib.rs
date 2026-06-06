//! Newt-Agent TUI — a lean chat + agentic-coding TUI in the spirit of Codex /
//! Claude Code, deliberately scoped to *chat and agentic coding* (not as
//! feature-rich). Splash + chat REPL + slash commands + ocap-gated tool use.
//! NOT a settings UI: configuration is plain `~/.newt/config.toml`
//! (see `newt config`). Additional features and the multi-agent matrix live in
//! the downstream `gilamonster-agent`, which inherits these crates.

mod mcp;
pub mod probe;
mod setup;
mod wizard;

use mcp::Mcp;
use newt_inference::retry::{with_backoff_notify, RetryPolicy};

/// Run the (non-interactive) setup wizard unconditionally — used by `newt init`.
/// Probes Ollama and (re)writes `~/.newt/config.toml`; edit that file for
/// anything else.
pub fn run_init(color: bool) -> anyhow::Result<()> {
    wizard::run_init(color)
}

/// Run the interactive setup wizard — used by `newt setup`. Asks where the
/// model runs (local Ollama or a remote DGX endpoint), probes for installed
/// models, and writes `~/.newt/config.toml` after a preview + confirmation.
pub fn run_setup(color: bool) -> anyhow::Result<()> {
    setup::run(color)
}

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
const NEWT_ORANGE_CT: CtColor = CtColor::Rgb {
    r: 220,
    g: 60,
    b: 20,
};

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

pub fn run_code(
    path: Option<&std::path::Path>,
    no_splash: bool,
    persona: Option<&str>,
) -> anyhow::Result<()> {
    let color = color_supported_with(&|k| std::env::var(k).ok());

    // First-run wizard: silent no-op if config already exists.
    wizard::maybe_run(color)?;

    let workspace = resolve_workspace(path);

    // `no_splash` is already resolved by the caller (CLI flags + config).
    // Print a compact inline header and go straight to chat — no alt screen,
    // no raw mode, scrolls naturally into history. Safe for SSH/tmux/pipes.
    let inline = no_splash;

    if inline {
        print_inline_header(&workspace, color);
        return run_chat(&workspace, color, persona);
    }

    // Default: full ANSI splash in alt screen — blinks off on Enter.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        Hide,
        Clear(ClearType::All),
        MoveTo(0, 0)
    )?;
    let cont = show_splash(&mut stdout, &workspace, color)?;
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);

    if cont {
        run_chat(&workspace, color, persona)?;
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
        (
            "ready — type a task, /help for commands, /exit to quit",
            true,
        ),
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
        (
            LOGO_120,
            LOGO_120_COLS,
            61u16,
            LOGO_120_COLS + STATUS_MIN_COLS + 2,
        ),
        (
            LOGO_FULL,
            LOGO_FULL_COLS,
            40u16,
            LOGO_FULL_COLS + STATUS_MIN_COLS + 2,
        ),
        (
            LOGO_40,
            LOGO_40_COLS,
            20u16,
            LOGO_40_COLS + STATUS_MIN_COLS + 2,
        ),
        (
            LOGO_20,
            LOGO_20_COLS,
            10u16,
            LOGO_20_COLS + STATUS_MIN_COLS + 2,
        ),
        (
            LOGO_10,
            LOGO_10_COLS,
            5u16,
            LOGO_10_COLS + STATUS_MIN_COLS + 2,
        ),
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
                .constraints([
                    Constraint::Fill(1),
                    Constraint::Length(w),
                    Constraint::Fill(1),
                ])
                .split(area);
            f.render_widget(Paragraph::new(Text::from(lines)), cols[1]);
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

/// Return today's date as `YYYY-MM-DD` using the system clock.
fn today_date() -> String {
    // Using std::time for a lightweight date without a chrono dep.
    // We only need YYYY-MM-DD so we derive it from epoch seconds manually.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Days since Unix epoch.
    let days = secs / 86400;
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

// ---------------------------------------------------------------------------
// File-descriptor hygiene — prevent EMFILE from killing rustyline
// ---------------------------------------------------------------------------

/// Mark every open fd above stderr (`fd > 2`) as `O_CLOEXEC` so that
/// subprocesses spawned by `run_command` (via agent-bridle / brush) do NOT
/// inherit the parent's terminal fd, history file handle, or socket fds.
///
/// Call this **after** rustyline and history are initialised so that their
/// fds are also marked. Safe to call multiple times — already-CLOEXEC fds
/// are skipped.
///
/// macOS returns `LONG_MAX` for `sysconf(_SC_OPEN_MAX)`; we cap the sweep
/// at 4096 which covers any realistic fd table.
#[cfg(unix)]
fn mark_fds_cloexec() {
    let max = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    let max_fd = if max > 0 {
        max.min(4096) as libc::c_int
    } else {
        256
    };
    for fd in 3..max_fd {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 && (flags & libc::FD_CLOEXEC == 0) {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

/// Probe whether the fd table has at least one free slot by attempting to
/// open `/dev/null`. Returns `false` when the process is at EMFILE — i.e.,
/// the next `open("/dev/tty")` by rustyline would fail and panic.
///
/// Uses only `std::fs` (no libc dep) so it compiles on all platforms.
fn terminal_fd_available() -> bool {
    std::fs::File::open("/dev/null").is_ok()
}

/// Whether to show "newt" / "you" labels before the carets.
fn verbose_mode() -> bool {
    std::env::var("NEWT_CHAT_STYLE")
        .map(|v| v.eq_ignore_ascii_case("verbose"))
        .unwrap_or(false)
}

/// Collect the basenames of all executables in the CLI-granted directories
/// (`--venv` and `--exec-path`). Used to widen the exec caveat at session start
/// so the agent can run tools from those directories without needing them in
/// `[tui.permissions] extra_exec` in the config file.
#[cfg(unix)]
fn scan_cli_exec_grants() -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let mut dirs: Vec<String> = Vec::new();
    if let Ok(venv) = std::env::var("NEWT_VENV") {
        dirs.push(format!("{venv}/bin"));
    }
    if let Ok(paths) = std::env::var("NEWT_EXEC_PATHS") {
        for p in paths.split(':') {
            if !p.is_empty() {
                dirs.push(p.to_string());
            }
        }
    }
    dirs.iter()
        .flat_map(|dir| std::fs::read_dir(dir).ok().into_iter().flatten())
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if !path.is_file() {
                return None;
            }
            let mode = path.metadata().ok()?.permissions().mode();
            if mode & 0o111 == 0 {
                return None;
            }
            path.file_name()?.to_str().map(str::to_string)
        })
        .collect()
}

#[cfg(not(unix))]
fn scan_cli_exec_grants() -> Vec<String> {
    Vec::new()
}

/// Build a shell prefix that exports venv/exec-path vars into the agent-bridle
/// confined shell.
///
/// Agent-bridle's confined shell does not inherit the host environment
/// (`do_not_inherit_env(true)`), so we inject `VIRTUAL_ENV` and prepend
/// venv/extra `bin/` dirs to `PATH` by prefixing every `run_command` cmd.
/// `NEWT_VENV` (set from `--venv` or auto-detected from `$VIRTUAL_ENV` by the
/// CLI) takes precedence; falls back to `$VIRTUAL_ENV` if the TUI was invoked
/// directly without going through the CLI's `dispatch`.
fn venv_cmd_prefix() -> Option<String> {
    let venv = std::env::var("NEWT_VENV")
        .or_else(|_| std::env::var("VIRTUAL_ENV"))
        .ok();
    let exec_paths = std::env::var("NEWT_EXEC_PATHS").ok();

    if venv.is_none() && exec_paths.is_none() {
        return None;
    }

    // sh single-quoting: wrap in '', escape any ' as '\''
    let q = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));

    // Build a list of dirs to prepend to PATH (venv/bin first, then exec-paths).
    let mut path_dirs: Vec<String> = Vec::new();
    let mut prefix = String::new();

    if let Some(ref venv) = venv {
        let venv_bin = format!("{venv}/bin");
        prefix.push_str(&format!("export VIRTUAL_ENV={}; ", q(venv)));
        path_dirs.push(venv_bin);
    }
    if let Some(ref paths) = exec_paths {
        for dir in paths.split(':') {
            if !dir.is_empty() {
                path_dirs.push(dir.to_string());
            }
        }
    }

    if !path_dirs.is_empty() {
        let quoted: Vec<String> = path_dirs.iter().map(|d| q(d)).collect();
        prefix.push_str(&format!("export PATH={}:\"$PATH\"; ", quoted.join(":")));
    }

    if prefix.is_empty() {
        None
    } else {
        Some(prefix)
    }
}

/// Whether per-round agent-loop diagnostics are enabled.
/// Set `NEWT_DEBUG=1` in the environment, or `[tui] debug = true` in config.
fn debug_mode(cfg: &newt_core::Config) -> bool {
    std::env::var("NEWT_DEBUG").is_ok() || cfg.tui.as_ref().and_then(|t| t.debug).unwrap_or(false)
}

/// Print a single-line debug diagnostic (dimmed, prefix `[debug]`).
/// Only called when `ChatCtx.debug` is true — guard at the call site.
fn print_debug(msg: &str, color: bool) {
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::DarkGrey),
            Print(format!("[debug] {msg}\n")),
            ResetColor,
        )
        .ok();
    } else {
        println!("[debug] {msg}");
    }
    io::stdout().flush().ok();
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

/// Read-only enforcement caveats (read anything; no write/exec/net) — the safe
/// default when nothing is configured or identity setup fails.
fn read_only_caveats(workspace: &str) -> newt_core::caveats::Caveats {
    newt_core::ToolPermissions {
        preset: newt_core::PermissionPreset::ReadOnly,
        extra_exec: Vec::new(),
        net: Vec::new(),
    }
    .to_caveats(workspace)
}

/// Lower the configured TUI permission policy to a `Caveats` value. With no
/// `[tui]` config the policy is **read-only** — never `Caveats::top()`. Pure in
/// its inputs, so the safe-default behavior is unit-testable.
///
/// CLI exec grants (`--venv`, `--exec-path`) are injected here: they widen the
/// exec scope at session-start so the agent can run tools in those directories
/// without requiring per-binary `extra_exec` entries in the config file. Only
/// `Scope::Only` is widened; `Scope::All` (FullAccess) is already unrestricted.
fn policy_for(tui: Option<newt_core::TuiConfig>, workspace: &str) -> newt_core::caveats::Caveats {
    let mut caveats = tui
        .map(|t| t.permissions.to_caveats(workspace))
        .unwrap_or_else(|| read_only_caveats(workspace));
    let extra = scan_cli_exec_grants();
    if !extra.is_empty() {
        if let newt_core::caveats::Scope::Only(ref mut set) = caveats.exec {
            set.extend(extra);
        }
    }
    caveats
}

/// Resolve the configured `[tui]` block, if any.
fn resolve_tui(cfg: &newt_core::Config) -> Option<newt_core::TuiConfig> {
    cfg.tui.clone()
}

/// Mint a signed operating key for `policy`, rooted in the per-user key at
/// `key_path` (generated on first use). Its caveats are provably `⊑` the user's
/// full authority, and it can only ever delegate *narrower* children.
fn mint_operating_key(
    key_path: &std::path::Path,
    policy: &newt_core::caveats::Caveats,
) -> Result<newt_identity::AgentKey, newt_identity::IdentityError> {
    let user = newt_identity::load_or_generate(key_path)?;
    let root = newt_identity::session_root(&user);
    newt_identity::attenuate(&root, policy)
}

/// The session's signed operating capability.
///
/// Established once from the per-user key (`~/.newt/identity.pem`) and the
/// configured preset, it enforces **in-session monotonic narrowing**:
/// re-applying a policy (e.g. after a config reload) can only ever *narrow* the live
/// authority, never widen it — widening would require re-rooting from the user
/// key, which only happens on a fresh session. The running agent can tighten its
/// own leash but never loosen it.
///
/// Safe by default: an absent config, or any identity error, yields read-only —
/// never `Caveats::top()`.
struct SessionCapability {
    /// The live operating key. `None` if the per-user key is unavailable; the
    /// capability then degrades to a plain caveats floor (still narrowing-only).
    op: Option<newt_identity::AgentKey>,
    caveats: newt_core::caveats::Caveats,
}

impl SessionCapability {
    /// Establish the session capability from the configured policy + per-user key.
    fn establish(
        tui: Option<newt_core::TuiConfig>,
        key_path: Option<&std::path::Path>,
        workspace: &str,
    ) -> Self {
        let policy = policy_for(tui, workspace);
        let op = key_path.and_then(|p| mint_operating_key(p, &policy).ok());
        let caveats = match &op {
            Some(k) => newt_identity::enforced_caveats(k).unwrap_or(policy),
            None => policy,
        };
        Self { op, caveats }
    }

    /// The active enforcement caveats the tool loop consults.
    fn caveats(&self) -> &newt_core::caveats::Caveats {
        &self.caveats
    }

    /// Mint a plugin-side envelope for a subprocess running under `role`
    /// with `child_caveats`, by delegating from the live operating key.
    ///
    /// **Issue #93:** when the TUI eventually spawns a subprocess
    /// plugin (today: in-process tool calls only), the resulting
    /// `AgentKey` it hands the plugin MUST chain back to the operator's
    /// `UserKey` from `~/.newt/identity.pem` — never a synthetic key
    /// minted at spawn time. This helper is the chokepoint the TUI's
    /// future subprocess-spawn path will route through.
    ///
    /// Returns:
    /// - `Some(Ok(envelope))` when the operating key is present and the
    ///   delegation succeeded — the envelope's cert chain roots back to
    ///   the operator.
    /// - `Some(Err(_))` when delegation refused (`child_caveats` would
    ///   amplify the operating key's authority).
    /// - `None` when the per-user key is unavailable (`SessionCapability`
    ///   degraded to a plain caveats floor).
    #[cfg_attr(not(test), allow(dead_code))]
    fn plugin_envelope_for(
        &self,
        role: &str,
        child_caveats: newt_core::Caveats,
    ) -> Option<std::result::Result<String, newt_identity::EnvelopeError>> {
        let op = self.op.as_ref()?;
        Some(newt_identity::delegate_for_plugin(op, role, child_caveats))
    }

    /// Re-apply a (possibly changed) policy, **narrowing-only**. Returns `true`
    /// if the request asked for *more* authority than the session currently
    /// holds and was therefore clamped (so the caller can tell the user a
    /// restart is required to widen).
    fn reapply(&mut self, tui: Option<newt_core::TuiConfig>, workspace: &str) -> bool {
        let requested = policy_for(tui, workspace);
        let narrowed = requested.meet(&self.caveats);
        let clamped = narrowed != requested;
        if let Some(op) = self.op.take() {
            match newt_identity::attenuate(&op, &narrowed)
                .and_then(|child| newt_identity::enforced_caveats(&child).map(|c| (child, c)))
            {
                Ok((child, c)) => {
                    self.op = Some(child);
                    self.caveats = c;
                }
                // Unreachable in practice (narrowed ⊑ op): keep the old key but
                // still apply the narrowed caveats.
                Err(_) => {
                    self.op = Some(op);
                    self.caveats = narrowed;
                }
            }
        } else {
            self.caveats = narrowed;
        }
        clamped
    }
}

#[cfg(test)]
mod caveat_policy_tests {
    use super::{policy_for, SessionCapability};
    // The `permits_*` adaptors live on `CaveatsExt` (post-#95).
    use newt_core::CaveatsExt;

    fn tui_with(preset: newt_core::PermissionPreset) -> newt_core::TuiConfig {
        newt_core::TuiConfig {
            permissions: newt_core::ToolPermissions {
                preset,
                extra_exec: Vec::new(),
                net: Vec::new(),
            },
            ..Default::default()
        }
    }

    #[test]
    fn absent_config_is_read_only() {
        // #86 regression: with no [tui] config the policy must be READ-ONLY,
        // never `Caveats::top()` (the old fallback granted full access).
        let policy = policy_for(None, "/ws");
        assert_ne!(policy, newt_core::caveats::Caveats::top());
        assert!(!policy.permits_exec("cargo"), "no exec when unconfigured");
        assert!(
            !policy.permits_fs_write("/ws/x"),
            "no write when unconfigured"
        );
        assert!(policy.permits_fs_read("/ws/x"), "reads still allowed");
    }

    #[test]
    fn establish_unconfigured_is_signed_read_only() {
        // #86 end-to-end: no config + a real (temp) key → read-only caveats via
        // the signed-capability path; the per-user key was generated.
        let dir = tempfile::TempDir::new().unwrap();
        let key = dir.path().join("identity.pem");
        let cap = SessionCapability::establish(None, Some(&key), "/ws");
        assert_ne!(*cap.caveats(), newt_core::caveats::Caveats::top());
        assert!(!cap.caveats().permits_exec("cargo"));
        assert!(cap.caveats().permits_fs_read("/ws/x"));
        assert!(key.exists(), "the per-user identity key was generated");
    }

    #[test]
    fn establish_without_key_is_read_only_policy() {
        let cap = SessionCapability::establish(None, None, "/ws");
        assert_ne!(*cap.caveats(), newt_core::caveats::Caveats::top());
        assert!(!cap.caveats().permits_exec("cargo"));
    }

    /// Issue #93: a subprocess plugin spawned from the TUI must inherit
    /// an `AgentKey` whose cert chain walks back to the operator's
    /// `UserKey` from `~/.newt/identity.pem`. This pins the chain-
    /// rooting property end to end through `SessionCapability`'s
    /// envelope-mint chokepoint.
    #[test]
    fn plugin_envelope_chain_roots_at_operator_userkey() {
        use base64::Engine;
        let dir = tempfile::TempDir::new().unwrap();
        let key_path = dir.path().join("identity.pem");
        let cap = SessionCapability::establish(
            Some(tui_with(newt_core::PermissionPreset::WorkspaceDev)),
            Some(&key_path),
            "/ws",
        );

        // Re-load the user key to get its fingerprint for the chain walk.
        let user = newt_identity::load_or_generate(&key_path).unwrap();
        let user_fp = user.fingerprint();

        // Plugin runs read-only — strictly narrower than WorkspaceDev.
        let plugin_caveats = newt_core::Caveats {
            fs_write: newt_core::Scope::none(),
            exec: newt_core::Scope::none(),
            ..cap.caveats().clone()
        };
        let envelope = cap
            .plugin_envelope_for("tui-spawned-plugin", plugin_caveats)
            .expect("operating key present → envelope path is available")
            .expect("attenuating delegation must succeed");

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&envelope)
            .unwrap();
        let leaf: agent_mesh_protocol::CertChain = serde_json::from_slice(&bytes).unwrap();
        leaf.verify().expect("plugin cert chain must verify");
        assert_eq!(
            leaf.user_fingerprint(),
            user_fp,
            "TUI-side plugin envelope must root at the operator's UserKey, \
             not a synthetic key"
        );
    }

    #[test]
    fn plugin_envelope_unavailable_without_operating_key() {
        // When the per-user key isn't on disk (None path), the TUI
        // degrades to a caveats-only floor. The plugin-spawn chokepoint
        // returns None — the caller must NOT manufacture an AgentKey
        // (issue #93). No synthetic-key fallback exists.
        let cap = SessionCapability::establish(None, None, "/ws");
        assert!(
            cap.plugin_envelope_for("tui-plugin", newt_core::Caveats::top())
                .is_none(),
            "no operating key → no envelope minted (issue #93: no synthetic fallback)"
        );
    }

    #[test]
    fn establish_configured_is_workspace_dev() {
        let dir = tempfile::TempDir::new().unwrap();
        let cap = SessionCapability::establish(
            Some(newt_core::TuiConfig::default()),
            Some(&dir.path().join("identity.pem")),
            "/ws",
        );
        assert!(cap.caveats().permits_exec("cargo"), "workspace-dev tools");
        assert!(!cap.caveats().permits_exec("rm"), "dangerous cmds denied");
    }

    #[test]
    fn reapply_narrows_but_cannot_widen() {
        // The headline runtime property: within a session, a config reload can tighten
        // authority but never loosen it (keyed off a temp identity).
        let dir = tempfile::TempDir::new().unwrap();
        let mut cap = SessionCapability::establish(
            Some(tui_with(newt_core::PermissionPreset::WorkspaceDev)),
            Some(&dir.path().join("identity.pem")),
            "/ws",
        );
        assert!(
            cap.caveats().permits_exec("cargo"),
            "starts at workspace-dev"
        );

        // Narrow to read-only: accepted, not clamped.
        let clamped = cap.reapply(Some(tui_with(newt_core::PermissionPreset::ReadOnly)), "/ws");
        assert!(!clamped, "narrowing is not a clamp");
        assert!(!cap.caveats().permits_exec("cargo"), "now read-only");

        // Try to widen back to workspace-dev: clamped, stays read-only.
        let clamped = cap.reapply(
            Some(tui_with(newt_core::PermissionPreset::WorkspaceDev)),
            "/ws",
        );
        assert!(clamped, "a widening request must be reported as clamped");
        assert!(
            !cap.caveats().permits_exec("cargo"),
            "authority must not widen within a session"
        );
    }

    #[test]
    fn reapply_without_key_still_narrows() {
        let mut cap = SessionCapability::establish(
            Some(tui_with(newt_core::PermissionPreset::WorkspaceDev)),
            None,
            "/ws",
        );
        assert!(cap.caveats().permits_exec("cargo"));
        let clamped = cap.reapply(Some(tui_with(newt_core::PermissionPreset::ReadOnly)), "/ws");
        assert!(!clamped);
        assert!(!cap.caveats().permits_exec("cargo"));
    }
}

/// Returns true if `full_path` is permitted by `scope`, using prefix matching
/// against the stored workspace-root strings.
///
/// The `Caveats` lattice stores workspace root strings (not individual file paths)
/// and uses exact-set semantics. The TUI adds path-prefix semantics here so that
/// "workspace root is permitted" translates to "any file under it is permitted".
fn tui_permits_path(scope: &newt_core::caveats::Scope<String>, full_path: &str) -> bool {
    match scope {
        newt_core::caveats::Scope::All => true,
        newt_core::caveats::Scope::Only(set) if set.is_empty() => false,
        newt_core::caveats::Scope::Only(set) => {
            set.iter().any(|root| full_path.starts_with(root.as_str()))
        }
    }
}

/// Print a capability-denial notice to the user.
fn print_denied(axis: &str, target: &str, color: bool) {
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::DarkGrey),
            Print(format!(
                "⊘  capability denied: {axis} does not permit '{target}'\n"
            )),
            ResetColor,
        )
        .ok();
    } else {
        println!("⊘  capability denied: {axis} does not permit '{target}'");
    }
    io::stdout().flush().ok();
}

fn run_chat(workspace: &str, color: bool, persona: Option<&str>) -> anyhow::Result<()> {
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
    let history_path = newt_core::Config::user_config_path().map(|p| p.with_file_name("history"));

    // Use the existing tokio runtime from main — block_in_place lets rustyline
    // block the thread while still allowing block_on() inside it.
    let rt = tokio::runtime::Handle::current();

    // Resolve config ONCE per session and reuse it for every read this turn.
    // It is re-read (`Config::resolve`) only after a slash command, the one
    // intentional refresh point — config.toml may have changed on disk.
    let mut cfg = newt_core::Config::resolve().unwrap_or_default();

    // Resolve the inference backend and permission caveats once at session
    // start.  Both are re-read after each slash command (config.toml on disk).
    let mut choice = resolve_backend_choice(&cfg);
    let (mut inf_url, mut inf_model) = (choice.url.clone(), choice.model.clone());
    let mut inf_kind = choice.kind;
    let mut inf_key = choice.api_key.clone();
    let key_path = newt_identity::default_key_path().ok();
    let mut cap = SessionCapability::establish(resolve_tui(&cfg), key_path.as_deref(), workspace);
    print_newt(
        &format!("v{VERSION} ready — {inf_model} @ {inf_url}  (Ctrl-D or /exit to quit)"),
        color,
        verbose,
    );

    // Connect to discovered MCP servers ONCE for the session (newt config +
    // Claude Code config). Failures are logged + skipped; their tools are added
    // to the agent's tool set, namespaced `server__tool`. `newt doctor` shows
    // the same discovery if a server is missing.
    let cfg_mcp_servers = cfg.mcp_servers.clone();
    let mut mcp =
        tokio::task::block_in_place(|| rt.block_on(Mcp::connect(workspace, &cfg_mcp_servers)));
    if !mcp.is_empty() {
        let summary = mcp
            .summary()
            .into_iter()
            .map(|(name, n)| format!("{name} ({n})"))
            .collect::<Vec<_>>()
            .join(", ");
        print_newt(&format!("MCP: {summary}"), color, verbose);
    }
    println!();

    let mut rl = rustyline::DefaultEditor::with_config(build_rl_config())?;
    if let Some(ref hp) = history_path {
        let _ = rl.load_history(hp);
    }
    // Mark all open fds (terminal, history file, sockets) as O_CLOEXEC so
    // subprocesses spawned by run_command don't inherit them. This is the
    // primary defence against EMFILE from cargo test / rustc worker floods.
    #[cfg(unix)]
    mark_fds_cloexec();

    let is_vi = build_rl_config().edit_mode() == rustyline::config::EditMode::Vi;
    let prompt = prompt_str(workspace, verbose, is_vi);

    // system prompt is built AFTER initialize_all (see below) so soul files are loaded.
    // Placeholder until then.
    let mut system: String;
    let persona_store = PersonaStore::default();
    let mut active_persona: Option<Persona> = match persona {
        Some(name) => Some(persona_store.load(name)?),
        None => None,
    };

    // Pluggable memory manager — replaces the old conv Vec.
    let mem_cfg = cfg.memory.clone().unwrap_or_default();
    let mut memory = {
        let mut mgr = newt_core::MemoryManager::new();
        // Soul provider first — sets the frozen identity block.
        let soul_override = mem_cfg.soul_file.as_ref().map(std::path::PathBuf::from);
        mgr.add_provider(newt_core::SoulProvider::new(soul_override));
        // History provider based on config.
        match mem_cfg.provider {
            newt_core::MemoryProviderKind::TokenBudget => {
                let max = mem_cfg.context_tokens.unwrap_or(8_192);
                mgr.add_provider(newt_core::TokenBudget::new(max, 0.80));
            }
            newt_core::MemoryProviderKind::Summarizing => {
                let max = mem_cfg.context_tokens.unwrap_or(8_192);
                // Wire the summariser to call the current model via the ACP loop.
                // The closure captures inf_url/inf_model at session start — model
                // switches mid-session will update on next session restart.
                let url = inf_url.clone();
                let model = inf_model.clone();
                let kind = inf_kind;
                let api_key = inf_key.clone();
                let s = newt_core::Summarizing::new(max).with_summarizer(
                    move |prompt: &str| -> anyhow::Result<String> {
                        let openai = kind == newt_core::BackendKind::Openai;
                        // OpenAI-compatible and Ollama use different paths and
                        // response shapes; pick per backend kind.
                        let chat_url = if openai {
                            format!("{}/v1/chat/completions", url.trim_end_matches('/'))
                        } else {
                            format!("{}/api/chat", url.trim_end_matches('/'))
                        };
                        let body = serde_json::json!({
                            "model": model,
                            "messages": [{"role": "user", "content": prompt}],
                            "stream": false,
                        });
                        let api_key = api_key.clone();
                        // We're called from sync_turn inside block_in_place,
                        // so we can use Handle::current().block_on here.
                        let json: serde_json::Value = tokio::task::block_in_place(|| {
                            tokio::runtime::Handle::current().block_on(async {
                                let mut req = reqwest::Client::builder()
                                    .timeout(std::time::Duration::from_secs(60))
                                    .build()?
                                    .post(&chat_url)
                                    .json(&body);
                                if let Some(key) = api_key {
                                    req = req.bearer_auth(key);
                                }
                                let resp = req.send().await?;
                                resp.json::<serde_json::Value>()
                                    .await
                                    .map_err(anyhow::Error::from)
                            })
                        })?;
                        let content = if openai {
                            json["choices"][0]["message"]["content"].as_str()
                        } else {
                            json["message"]["content"].as_str()
                        };
                        Ok(content.unwrap_or("(summary unavailable)").to_string())
                    },
                );
                mgr.add_provider(s);
            }
            _ => {
                mgr.add_provider(newt_core::RollingWindow::new(mem_cfg.window));
            }
        }
        // NoteStore is always active — manages system-prompt injection only.
        mgr.add_provider(newt_core::NoteStore::default_path());
        mgr
    };
    let ctx = newt_core::SessionContext {
        workspace: workspace.to_string(),
        session_id: format!(
            "{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ),
    };
    tokio::task::block_in_place(|| rt.block_on(memory.initialize_all(&ctx)));

    // Build system prompt now that SoulProvider has loaded its soul file.
    system = rebuild_system_prompt(workspace, &memory, active_persona.as_ref());

    loop {
        // rustyline can panic (assertion `fd != -1`) when the terminal file
        // descriptor becomes invalid — most commonly from file-descriptor
        // exhaustion after spawning many subprocesses (e.g., `cargo test`
        // with multiple compile workers). Without this guard the panic
        // propagates through a non-unwindable tokio boundary and the process
        // aborts with no useful message.
        //
        // `catch_unwind` catches the panic before it reaches that boundary and
        // converts it into a clean exit. `AssertUnwindSafe` is safe here:
        // `DefaultEditor` state may be inconsistent after a panic, but we
        // immediately `break` out of the loop and drop it rather than
        // continuing to use it.
        // Layer 2: probe for EMFILE before rustyline tries to open /dev/tty.
        // Catching the panic (Layer 3 / PR #184) remains as a last resort, but
        // this check fires first and gives a cleaner message when the fd table
        // is already full before readline even starts.
        if !terminal_fd_available() {
            let _ = disable_raw_mode();
            eprintln!("\nnewt: EMFILE — file descriptor table is full.");
            eprintln!("      Too many subprocesses (e.g. cargo test workers) inherited fds.");
            eprintln!(
                "      Restart newt. The O_CLOEXEC fix prevents recurrence on rebuilt binaries."
            );
            break;
        }

        let readline_result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rl.readline(&prompt)));
        let readline_result = match readline_result {
            Ok(r) => r,
            Err(_panic) => {
                let _ = disable_raw_mode();
                eprintln!("\nnewt: terminal error — readline panicked (likely fd exhaustion).");
                eprintln!("      Restart newt. If this recurs, reduce concurrent subprocesses.");
                break;
            }
        };
        match readline_result {
            Ok(line) => {
                let task = line.trim().to_string();
                if task.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(&task);
                println!();
                if task.starts_with('/') {
                    // Commands that need direct access to `memory` are handled here
                    // before delegating to the generic slash dispatcher.
                    if task.trim_start_matches('/').starts_with("memory") {
                        let usage = memory.usage();
                        if usage.is_empty() {
                            print_newt("No memory usage data available.", color, verbose);
                        } else {
                            print_newt("Context window usage:", color, verbose);
                            for (label, cur, max) in &usage {
                                let pct = if *max > 0 { cur * 100 / max } else { 0 };
                                println!("  {label}: {cur}/{max}  ({pct}%)");
                            }
                        }
                        println!();
                        continue;
                    }
                    if let Some(fact) = task.trim_start_matches('/').strip_prefix("remember ") {
                        // Find NoteStore in the manager and add the fact.
                        // We reach it via a best-effort downcast approach using a
                        // dedicated add_note helper on MemoryManager.
                        match memory.add_note(fact) {
                            Ok(()) => print_newt(&format!("Noted: {fact}"), color, verbose),
                            Err(e) => print_newt(&format!("error: {e}"), color, verbose),
                        }
                        println!();
                        continue;
                    }
                    if task.trim_start_matches('/') == "new" {
                        let msg = handle_new_conversation(
                            workspace,
                            &mut memory,
                            &mut system,
                            active_persona.as_ref(),
                        );
                        print_newt(&msg, color, verbose);
                        if let Some(ref hp) = history_path {
                            let _ = rl.save_history(hp);
                        }
                        println!();
                        continue;
                    }
                    let slash_body = task.trim_start_matches('/');
                    if slash_body == "persona" || slash_body.starts_with("persona ") {
                        match handle_persona_command(
                            &task,
                            workspace,
                            &persona_store,
                            &mut memory,
                            &mut system,
                            &mut active_persona,
                        ) {
                            Ok(msg) => print_newt(&msg, color, verbose),
                            Err(e) => print_newt(&format!("error: {e}"), color, verbose),
                        }
                        if let Some(ref hp) = history_path {
                            let _ = rl.save_history(hp);
                        }
                        println!();
                        continue;
                    }
                    let cont = dispatch_slash(&task, workspace, color, verbose)?;
                    if let Some(ref hp) = history_path {
                        let _ = rl.save_history(hp);
                    }
                    // Skip config reload and terminal reinit when exiting — unnecessary
                    // work that can hang if the terminal is in a degraded state.
                    if !cont {
                        break;
                    }
                    // Re-read config after a slash command (config.toml may have changed).
                    // This is the ONE intentional refresh — re-resolve `cfg` so the
                    // session picks up edits, then derive everything from it.
                    // Permissions can only NARROW within a session; a widening
                    // request is clamped (restart to widen — see SessionCapability).
                    cfg = newt_core::Config::resolve().unwrap_or_default();
                    choice = resolve_backend_choice(&cfg);
                    inf_url = choice.url.clone();
                    inf_model = choice.model.clone();
                    inf_kind = choice.kind;
                    inf_key = choice.api_key.clone();
                    if cap.reapply(resolve_tui(&cfg), workspace) {
                        print_newt(
                            "permissions can only narrow within a session — restart newt to widen",
                            color,
                            verbose,
                        );
                    }
                    let fresh_cfg = build_rl_config();
                    rl = rustyline::DefaultEditor::with_config(fresh_cfg)?;
                    if let Some(ref hp) = history_path {
                        let _ = rl.load_history(hp);
                    }
                } else if matches!(task.as_str(), "exit" | "quit") {
                    break;
                } else {
                    print_thinking(color);
                    let t0 = std::time::Instant::now();

                    // Build message list from memory manager.
                    let messages = memory.build_messages(&system, &task);
                    let response = tokio::task::block_in_place(|| {
                        rt.block_on(chat_complete(
                            ChatCtx {
                                url: &inf_url,
                                model: &inf_model,
                                kind: inf_kind,
                                api_key: inf_key.as_deref(),
                                messages: &messages,
                                task: &task,
                                workspace,
                                color,
                                caveats: cap.caveats(),
                                max_tool_rounds: max_tool_rounds(&cfg),
                                tool_output_lines: tool_output_lines(&cfg),
                                debug: debug_mode(&cfg),
                                num_ctx: num_ctx(&cfg),
                                connect_timeout_secs: connect_timeout_secs(&cfg),
                                inference_timeout_secs: inference_timeout_secs(&cfg),
                                mid_loop_trim_threshold: mid_loop_trim_threshold(&cfg),
                                build_check_cmd: build_check_cmd(&cfg),
                            },
                            &mut mcp,
                        ))
                    });

                    let elapsed = t0.elapsed();
                    erase_line();
                    match response {
                        Ok((reply, was_streamed, usage, hallucinations)) => {
                            if !was_streamed {
                                print_newt(&reply, color, verbose);
                            }
                            // Single TurnMetrics used for both memory sync and display.
                            let pricing = cfg.pricing.clone().unwrap_or_default();
                            let metrics = newt_core::TurnMetrics {
                                elapsed_ms: elapsed.as_millis() as u64,
                                usage,
                                cost_usd: pricing.estimate_cost(&inf_model, usage.as_ref()),
                                model_id: inf_model.clone(),
                                endpoint: inf_url.clone(),
                                hallucinations,
                            };
                            tokio::task::block_in_place(|| {
                                rt.block_on(memory.sync_all(&task, &reply, &metrics));
                            });
                            print_metrics(&metrics, color);
                            // Append to usage log and enforce rotation policy.
                            if let Some(log) = newt_core::Config::user_config_path()
                                .map(|p| p.with_file_name("usage.jsonl"))
                            {
                                let policy = cfg.logs.as_ref().cloned().unwrap_or_default();
                                metrics.append_to_log_with_policy(&log, &policy);
                            }
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
fn resolve_backend_config(cfg: &newt_core::Config) -> (String, String) {
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
            cfg.dgx
                .as_ref()
                .and_then(|d| d.nodes.first())
                .and_then(|n| n.ollama.clone())
        })
        .unwrap_or_else(|| "http://localhost:11434".into());

    let model = std::env::var("NEWT_DGX_MODEL")
        .ok()
        .or_else(|| cfg.dgx.as_ref().and_then(|d| d.active_model.clone()))
        .unwrap_or_else(|| "llama3.1:8b".into());

    (url, model)
}

/// The inference backend the TUI session should talk to: endpoint, model,
/// wire protocol, and (for authenticated OpenAI-compatible endpoints) the
/// resolved bearer token.
struct BackendChoice {
    url: String,
    model: String,
    kind: newt_core::BackendKind,
    api_key: Option<String>,
}

/// Resolve the backend for the TUI. An explicit `kind = "openai"` backend in
/// the config (`~/.newt/config.toml`) wins — endpoint/model/auth come straight
/// from it. Otherwise we fall back to the historical Ollama/DGX resolution
/// ([`resolve_backend_config`]), which the rest of the TUI already understands.
fn resolve_backend_choice(cfg: &newt_core::Config) -> BackendChoice {
    if let Some(b) = cfg
        .backends
        .iter()
        .find(|b| b.kind == newt_core::BackendKind::Openai)
    {
        return BackendChoice {
            url: b.endpoint.clone(),
            model: b.model.clone(),
            kind: newt_core::BackendKind::Openai,
            api_key: b.resolve_api_key(),
        };
    }
    let (url, model) = resolve_backend_config(cfg);
    BackendChoice {
        url,
        model,
        kind: newt_core::BackendKind::Ollama,
        api_key: None,
    }
}

/// Build a system prompt with workspace context so the model knows the project.
// build_system_prompt_with_soul is used directly now; this wrapper kept for tests.
#[allow(dead_code)]
fn build_system_prompt(workspace: &str) -> String {
    build_system_prompt_with_soul(workspace, None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Persona {
    name: String,
    prompt: String,
    path: std::path::PathBuf,
}

impl Persona {
    fn description(&self) -> String {
        self.prompt
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("")
            .trim_start_matches('#')
            .trim()
            .chars()
            .take(96)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersonaSummary {
    name: String,
    description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersonaStore {
    dir: std::path::PathBuf,
}

impl PersonaStore {
    const DEFAULT_NAME: &'static str = "coder";

    fn default_dir() -> std::path::PathBuf {
        newt_core::Config::user_config_path()
            .map(|p| p.with_file_name("personas"))
            .unwrap_or_else(|| std::path::PathBuf::from("personas"))
    }

    fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn default() -> Self {
        Self::new(Self::default_dir())
    }

    fn load(&self, name: &str) -> anyhow::Result<Persona> {
        self.ensure_defaults_if_empty()?;
        let name = normalize_persona_name(name)?;
        let path = self.dir.join(format!("{name}.md"));
        let prompt = match std::fs::read_to_string(&path) {
            Ok(prompt) => prompt,
            Err(_) => anyhow::bail!("unknown persona `{name}`\n{}", self.list_message()?),
        };
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() {
            anyhow::bail!("persona `{name}` is empty: {}", path.display());
        }
        Ok(Persona { name, prompt, path })
    }

    fn list(&self) -> anyhow::Result<Vec<PersonaSummary>> {
        self.ensure_defaults_if_empty()?;
        let mut personas = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let prompt = std::fs::read_to_string(&path).unwrap_or_default();
            if prompt.trim().is_empty() {
                continue;
            }
            let persona = Persona {
                name: name.to_string(),
                prompt,
                path: path.clone(),
            };
            let description = persona.description();
            personas.push(PersonaSummary {
                name: persona.name,
                description,
            });
        }
        personas.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(personas)
    }

    fn list_message(&self) -> anyhow::Result<String> {
        let personas = self.list()?;
        let mut out = format!("Available personas in {}:", self.dir.display());
        if personas.is_empty() {
            out.push_str("\n  (none)");
        } else {
            for p in personas {
                out.push_str(&format!("\n  {} - {}", p.name, p.description));
            }
        }
        Ok(out)
    }

    fn ensure_defaults_if_empty(&self) -> anyhow::Result<()> {
        if self.has_persona_files()? {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(
            self.dir.join(format!("{}.md", Self::DEFAULT_NAME)),
            default_coder_persona(),
        )?;
        Ok(())
    }

    fn has_persona_files(&self) -> anyhow::Result<bool> {
        if !self.dir.exists() {
            return Ok(false);
        }
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn default_coder_persona() -> &'static str {
    newt_core::DEFAULT_SOUL
}

fn normalize_persona_name(name: &str) -> anyhow::Result<String> {
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!("persona names may only contain letters, numbers, '-' and '_'");
    }
    Ok(name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PersonaCommand {
    List,
    Show,
    Clear,
    Set(String),
}

fn parse_persona_command(input: &str) -> anyhow::Result<PersonaCommand> {
    let body = input.trim().trim_start_matches('/').trim();
    let mut parts = body.split_whitespace();
    match parts.next() {
        Some("persona") => {}
        _ => anyhow::bail!("not a persona command"),
    }

    match parts.next() {
        None | Some("show") => Ok(PersonaCommand::Show),
        Some("list") => Ok(PersonaCommand::List),
        Some("clear" | "off") => Ok(PersonaCommand::Clear),
        Some("default") => Ok(PersonaCommand::Set(PersonaStore::DEFAULT_NAME.into())),
        Some("set") => match parts.next() {
            Some(name) => Ok(PersonaCommand::Set(name.to_string())),
            None => anyhow::bail!("usage: /persona set <name>"),
        },
        Some(name) => Ok(PersonaCommand::Set(name.to_string())),
    }
}

/// Build the progressive-disclosure skills index block for the system prompt
/// from the skills found across `skills_dirs` (names + descriptions only, never
/// bodies), or `None` when no skills are installed. Uses the same ordered,
/// first-directory-wins union as discovery. Split out from
/// [`build_system_prompt_with_soul`] so the injection can be unit-tested against
/// controlled directories without mutating the process-wide `$HOME`.
fn skills_index_for_prompt(skills_dirs: &[std::path::PathBuf]) -> Option<String> {
    newt_skills::index_block(&newt_skills::discover_paths(skills_dirs))
}

fn build_system_prompt_with_soul(workspace: &str, soul: Option<&str>) -> String {
    build_system_prompt_with_persona(workspace, soul, None)
}

fn build_system_prompt_with_persona(
    workspace: &str,
    soul: Option<&str>,
    persona: Option<&Persona>,
) -> String {
    // Fall back to the single canonical identity in newt-core rather than a
    // private copy, so the built-in tool list can't drift between the two.
    let identity = soul.unwrap_or(newt_core::DEFAULT_SOUL);
    let mut ctx = format!("{identity}\n\nWorkspace: {workspace}\n");
    if let Some(persona) = persona {
        ctx.push_str(&format!(
            "\nActive persona: {}\n{}\n",
            persona.name, persona.prompt
        ));
    }

    // Progressive disclosure: inject ONLY the skills index (one
    // `name: description` line per installed skill) — never the bodies.
    // Bodies load on demand when the model calls the `use_skill` tool. Skills
    // come from the configured search path (`[skills].search`, default
    // `~/.newt/skills`); a missing dir contributes nothing.
    let skills_dirs = newt_core::Config::resolve()
        .map(|c| c.skill_search_dirs())
        .unwrap_or_default();
    if let Some(index) = skills_index_for_prompt(&skills_dirs) {
        ctx.push('\n');
        ctx.push_str(&index);
    }

    // Directory listing (top-level, no hidden files)
    if let Ok(mut entries) = std::fs::read_dir(workspace) {
        let mut names: Vec<String> = entries
            .by_ref()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    None
                } else {
                    Some(name)
                }
            })
            .collect();
        names.sort();
        ctx.push_str("\nFiles:\n");
        for name in names.iter().take(40) {
            ctx.push_str(&format!("  {name}\n"));
        }
    }

    // README (truncated)
    for readme in ["README.md", "readme.md", "README.txt"] {
        let path = std::path::Path::new(workspace).join(readme);
        if let Ok(text) = std::fs::read_to_string(&path) {
            let excerpt: String = text.chars().take(3000).collect();
            ctx.push_str(&format!("\n{readme}:\n{excerpt}\n"));
            if text.len() > 3000 {
                ctx.push_str("...[truncated]\n");
            }
            break;
        }
    }

    // Recent git log
    let log = std::process::Command::new("git")
        .args(["-C", workspace, "log", "--oneline", "-10"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    if !log.is_empty() {
        ctx.push_str(&format!("\nRecent commits:\n{log}"));
    }

    ctx
}

// ---------------------------------------------------------------------------
// Tool-use loop — the real agentic core
// ---------------------------------------------------------------------------

fn rebuild_system_prompt(
    workspace: &str,
    memory: &newt_core::MemoryManager,
    persona: Option<&Persona>,
) -> String {
    let soul_additions = memory.build_system_prompt_additions();
    let soul_text = if soul_additions.is_empty() {
        None
    } else {
        Some(soul_additions.as_str())
    };
    build_system_prompt_with_persona(workspace, soul_text, persona)
}

fn persona_status(active: Option<&Persona>) -> String {
    match active {
        Some(persona) => format!(
            "Active persona: {} - {} ({})",
            persona.name,
            persona.description(),
            persona.path.display()
        ),
        None => "No active persona.".to_string(),
    }
}

fn persona_list(store: &PersonaStore) -> anyhow::Result<String> {
    store.list_message()
}

fn reset_conversation(
    workspace: &str,
    memory: &mut newt_core::MemoryManager,
    system: &mut String,
    active_persona: Option<&Persona>,
) {
    memory.reset_all();
    *system = rebuild_system_prompt(workspace, memory, active_persona);
}

fn new_conversation_message(active_persona: Option<&Persona>) -> String {
    match active_persona {
        Some(persona) => format!(
            "Started a new conversation with persona `{}`.",
            persona.name
        ),
        None => "Started a new conversation.".to_string(),
    }
}

fn handle_new_conversation(
    workspace: &str,
    memory: &mut newt_core::MemoryManager,
    system: &mut String,
    active_persona: Option<&Persona>,
) -> String {
    reset_conversation(workspace, memory, system, active_persona);
    new_conversation_message(active_persona)
}

fn handle_persona_command(
    input: &str,
    workspace: &str,
    store: &PersonaStore,
    memory: &mut newt_core::MemoryManager,
    system: &mut String,
    active_persona: &mut Option<Persona>,
) -> anyhow::Result<String> {
    match parse_persona_command(input)? {
        PersonaCommand::List => persona_list(store),
        PersonaCommand::Show => Ok(persona_status(active_persona.as_ref())),
        PersonaCommand::Clear => {
            *active_persona = None;
            reset_conversation(workspace, memory, system, active_persona.as_ref());
            Ok("Started a new conversation with no active persona.".to_string())
        }
        PersonaCommand::Set(name) => {
            let persona = store.load(&name)?;
            *active_persona = Some(persona);
            reset_conversation(workspace, memory, system, active_persona.as_ref());
            Ok(new_conversation_message(active_persona.as_ref()))
        }
    }
}

fn tool_definitions() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "run_command",
                "description": "Run a shell command in the workspace directory and return its output",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command to run" }
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read the contents of a file in the workspace",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path relative to workspace root" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write or overwrite a file in the workspace. \
                                WARNING: use edit_file instead when modifying an existing file — \
                                write_file replaces the entire contents and will fail if the new \
                                content is significantly shorter than the original (shrink guard). \
                                Only use write_file for new files or full rewrites you have \
                                explicitly generated in their entirety.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path relative to workspace root" },
                        "content": { "type": "string", "description": "The complete new file contents" }
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "Make a targeted edit to an existing file by replacing one exact \
                                string with another. Safer than write_file for modifying existing \
                                files — you only generate the change, not the whole file. \
                                Fails with a clear error if old_string is not found or matches \
                                multiple times (add more surrounding context to make it unique).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path relative to workspace root" },
                        "old_string": { "type": "string", "description": "Exact string to find and replace (must match exactly once)" },
                        "new_string": { "type": "string", "description": "Replacement string" }
                    },
                    "required": ["path", "old_string", "new_string"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List files in a directory",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Directory path relative to workspace root (use '.' for root)" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "use_skill",
                "description": "Load a skill's full procedural instructions on demand. The system prompt lists the available skills (name + description); call this with a skill's name to get its complete SKILL.md body plus the paths of any bundled files (scripts/templates) you can read or run.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "The skill name as shown in the 'Available skills' index" }
                    },
                    "required": ["name"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "Fetch an http(s) URL and return its main content as clean markdown. Use this to read documentation, issues, or pages the task references. Reachable hosts are gated by the session's network capability; the returned text is untrusted page content.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "The http(s) URL to fetch" },
                        "max_bytes": { "type": "integer", "description": "Optional cap on bytes downloaded (default 5 MiB, max 25 MiB)" }
                    },
                    "required": ["url"]
                }
            }
        }
    ])
}

/// Retry policy for TUI inference calls: more patient than the hosted-API
/// default because local DGX nodes can drop for 30–60 s under load.
/// Total resilience window: ~90 s (2+4+8+16+30+30 s between attempts).
/// All thresholds are overridable via the standard `NEWT_HTTP_*` env vars.
fn tui_retry_policy() -> RetryPolicy {
    RetryPolicy::for_local_inference()
}

/// Print a visible retry indicator to the TUI so the user knows why there's
/// a pause rather than seeing a silent hang.
fn print_retry_indicator(attempt: u32, delay: std::time::Duration, color: bool) {
    let delay_s = delay.as_secs_f32();
    let msg = format!("  ↻ connection lost — retrying in {delay_s:.1}s (attempt {attempt})…\n");
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::Rgb {
                r: 200,
                g: 140,
                b: 0
            }),
            Print(&msg),
            ResetColor,
        )
        .ok();
    } else {
        print!("{msg}");
    }
    io::stdout().flush().ok();
}

/// Direct tool names the model must call as tool invocations, never as shell
/// commands passed to `run_command`.
const DIRECT_TOOL_NAMES: &[&str] = &[
    "list_dir",
    "read_file",
    "write_file",
    "edit_file",
    "use_skill",
    "web_fetch",
];

/// Returns `true` if a tool call looks like a hallucination:
/// - `run_command` called with a tool name as the shell command, or
/// - An unknown tool name (excluding MCP-namespaced `server__tool` names).
fn is_hallucination(tool_name: &str, args: &serde_json::Value) -> bool {
    if tool_name == "run_command" {
        let cmd = args["command"].as_str().unwrap_or("");
        let first = cmd.split_ascii_whitespace().next().unwrap_or("");
        return DIRECT_TOOL_NAMES.contains(&first);
    }
    // MCP tools are namespaced with `__` — never treat them as hallucinations.
    if tool_name.contains("__") {
        return false;
    }
    !matches!(
        tool_name,
        "run_command"
            | "list_dir"
            | "read_file"
            | "write_file"
            | "edit_file"
            | "use_skill"
            | "web_fetch"
    )
}

/// Trim a message list for the cap-exit summary: keep the first `head` messages
/// (system prompt + original task) and the last `tail` messages (recent rounds).
/// Inserts a single placeholder when the middle is dropped so the model knows
/// context was omitted rather than assuming the task was simpler than it is.
fn trim_for_summary(
    messages: &[serde_json::Value],
    head: usize,
    tail: usize,
) -> Vec<serde_json::Value> {
    if messages.len() <= head + tail {
        return messages.to_vec();
    }
    let dropped = messages.len() - head - tail;
    let mut result = Vec::with_capacity(head + 1 + tail);
    result.extend_from_slice(&messages[..head]);
    result.push(serde_json::json!({
        "role": "user",
        "content": format!(
            "[{dropped} earlier tool-call messages omitted to keep context within model limits]"
        ),
    }));
    result.extend_from_slice(&messages[messages.len() - tail..]);
    // Anthropic/Bedrock requires every tool_use block to be followed by its
    // tool_result. Trimming can orphan tool_calls — remove them so strict
    // backends don't reject the whole request with 400 Bad Request.
    repair_orphaned_tool_calls(&mut result);
    result
}

/// Remove or neutralise tool-call/result messages that form an incomplete pair
/// after `trim_for_summary` cuts the middle of a conversation.
///
/// Two failure modes that Anthropic/Bedrock reject with 400:
///
/// 1. **Partial results** — an assistant message has `tool_calls: [tc1, tc2]` but
///    only `tc1`'s `role="tool"` result survived trimming.  LiteLLM converts
///    *both* IDs to Bedrock `tool_use` blocks; Bedrock then complains that
///    `tc2` has no matching `tool_result`.  The previous check (`next message
///    is role="tool"`) was not sufficient — it didn't verify every ID.
///
/// 2. **Orphaned results** — a `role="tool"` message lands at the start of the
///    tail with no preceding assistant `tool_calls` (its assistant turn was
///    dropped).  Some LiteLLM/Bedrock versions reject unmatched results too.
///
/// Strategy:
///   Pass 1 — for each assistant with `tool_calls`, verify every ID has a
///             `role="tool"` result anywhere in the list; if any are missing,
///             strip **all** `tool_calls` from that assistant turn.
///   Pass 2 — remove every `role="tool"` message whose `tool_call_id` is not
///             referenced by any remaining assistant `tool_calls`.
fn repair_orphaned_tool_calls(messages: &mut Vec<serde_json::Value>) {
    // Build the set of tool_call IDs for which a role="tool" result exists.
    let result_ids: std::collections::HashSet<String> = messages
        .iter()
        .filter(|m| m["role"].as_str() == Some("tool"))
        .filter_map(|m| m["tool_call_id"].as_str().map(|s| s.to_string()))
        .collect();

    // Pass 1: determine which assistant messages need their tool_calls stripped,
    // then apply the changes in a second pass to avoid conflicting borrows.
    let roles: Vec<Option<String>> = messages
        .iter()
        .map(|m| m["role"].as_str().map(|s| s.to_string()))
        .collect();

    let strip_indices: std::collections::HashSet<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, msg)| {
            if msg["role"].as_str() != Some("assistant") {
                return None;
            }
            let tool_calls = msg["tool_calls"].as_array()?;
            if tool_calls.is_empty() {
                return None;
            }
            let ids: Vec<String> = tool_calls
                .iter()
                .filter_map(|tc| tc["id"].as_str().map(|s| s.to_string()))
                .collect();
            let should_strip = if ids.is_empty() {
                // No IDs: fall back to positional check.
                roles.get(i + 1).and_then(|r| r.as_deref()) != Some("tool")
            } else {
                !ids.iter().all(|id| result_ids.contains(id))
            };
            should_strip.then_some(i)
        })
        .collect();

    for i in strip_indices {
        if let Some(obj) = messages[i].as_object_mut() {
            obj.remove("tool_calls");
            obj.entry("content")
                .or_insert_with(|| serde_json::json!("[tool calls omitted]"));
        }
    }

    // Pass 2: remove role="tool" messages with no matching assistant tool_calls.
    let live_call_ids: std::collections::HashSet<String> = messages
        .iter()
        .filter(|m| m["role"].as_str() == Some("assistant"))
        .filter_map(|m| m["tool_calls"].as_array())
        .flat_map(|tc| tc.iter())
        .filter_map(|tc| tc["id"].as_str().map(|s| s.to_string()))
        .collect();

    messages.retain(|m| {
        if m["role"].as_str() != Some("tool") {
            return true;
        }
        // Keep tool results with no ID (malformed but harmless).
        // Only drop results whose explicit ID has no matching live tool_call.
        match m["tool_call_id"].as_str() {
            Some(id) if !id.is_empty() => live_call_ids.contains(id),
            _ => true,
        }
    });
}

/// Merge two optional token usage readings (e.g. accumulated across rounds).
fn merge_usage(
    acc: Option<newt_core::TokenUsage>,
    new: Option<newt_core::TokenUsage>,
) -> Option<newt_core::TokenUsage> {
    match (acc, new) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

/// Extract token usage from an Ollama non-streaming response (top-level
/// `prompt_eval_count` / `eval_count` fields).
fn ollama_usage(json: &serde_json::Value) -> Option<newt_core::TokenUsage> {
    let input = json["prompt_eval_count"].as_u64()? as u32;
    let output = json["eval_count"].as_u64()? as u32;
    Some(newt_core::TokenUsage {
        input_tokens: input,
        output_tokens: output,
    })
}

/// The built-in tool definitions plus every connected MCP server's tools
/// (namespaced `server__tool`). This is what the agent loop advertises to the
/// model so it can call remote MCP tools alongside the built-ins.
fn merged_tool_definitions(mcp: &Mcp) -> serde_json::Value {
    let mut defs = match tool_definitions() {
        serde_json::Value::Array(a) => a,
        other => vec![other],
    };
    defs.extend(mcp.tool_defs());
    serde_json::Value::Array(defs)
}

/// Print a tool-call header so the user can see what the agent is doing.
fn print_tool_call(name: &str, detail: &str, color: bool) {
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(NEWT_ORANGE_CT),
            Print(format!("⚙  {name}")),
            ResetColor,
            SetForegroundColor(CtColor::DarkGrey),
            Print(format!(": {detail}\n")),
            ResetColor,
        )
        .ok();
    } else {
        println!("⚙  {name}: {detail}");
    }
    io::stdout().flush().ok();
}

/// Read the tool-output line limit from config (default 20, 0 = unlimited).
fn tool_output_lines(cfg: &newt_core::Config) -> usize {
    cfg.tui.as_ref().map(|t| t.tool_output_lines).unwrap_or(20)
}

/// Maximum tool-call rounds per turn, from `[tui].max_tool_rounds`.
/// Defaults to 25 when there's no `[tui]` table or no config file.
fn max_tool_rounds(cfg: &newt_core::Config) -> usize {
    cfg.tui.as_ref().map(|t| t.max_tool_rounds).unwrap_or(25)
}

/// Ollama context-window cap. Resolution order:
///   1. `NEWT_NUM_CTX` env var (set by `--num-ctx` CLI flag or manually)
///   2. `[tui] num_ctx` in config
///   3. None → Ollama uses the model's compiled-in default
fn num_ctx(cfg: &newt_core::Config) -> Option<u32> {
    if let Ok(val) = std::env::var("NEWT_NUM_CTX") {
        if let Ok(n) = val.trim().parse::<u32>() {
            return Some(n);
        }
    }
    cfg.tui.as_ref().and_then(|t| t.num_ctx)
}

/// TCP connect timeout from `[tui].connect_timeout_secs` (default 5).
fn connect_timeout_secs(cfg: &newt_core::Config) -> u64 {
    cfg.tui
        .as_ref()
        .map(|t| t.connect_timeout_secs)
        .unwrap_or(5)
}

/// Full inference timeout from `[tui].inference_timeout_secs` (default 120).
fn inference_timeout_secs(cfg: &newt_core::Config) -> u64 {
    cfg.tui
        .as_ref()
        .map(|t| t.inference_timeout_secs)
        .unwrap_or(120)
}

/// Ollama keep_alive from `[tui].keep_alive` (default "5m").
fn keep_alive_str(cfg: &newt_core::Config) -> String {
    cfg.tui
        .as_ref()
        .map(|t| t.keep_alive.clone())
        .unwrap_or_else(|| "5m".to_string())
}

/// Mid-loop message-trim threshold from `[tui].mid_loop_trim_threshold` (default 40).
///
/// Clamped to `max_tool_rounds - 3` so the safety valve always fires before the
/// round ceiling — even when the config has threshold > max_rounds (e.g. default
/// threshold=40 with max_rounds=25 meant trimming never triggered).
fn mid_loop_trim_threshold(cfg: &newt_core::Config) -> usize {
    let threshold = cfg
        .tui
        .as_ref()
        .map(|t| t.mid_loop_trim_threshold)
        .unwrap_or(40);
    threshold.min(max_tool_rounds(cfg).saturating_sub(3))
}

/// Build-check command from `[tui].build_check_cmd`. `None` means no auto-check.
fn build_check_cmd(cfg: &newt_core::Config) -> Option<String> {
    cfg.tui.as_ref().and_then(|t| t.build_check_cmd.clone())
}

/// Run the configured build-check command in `workspace` and return a compact
/// result string appended to the tool output so the model sees it immediately.
fn run_build_check(cmd: &str, workspace: &str) -> String {
    let result = std::process::Command::new("sh")
        .args(["-c", cmd])
        .current_dir(workspace)
        .output();
    match result {
        Ok(out) if out.status.success() => "  ✓ build check passed".to_string(),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let combined = format!("{stdout}{stderr}");
            let excerpt: String = combined.lines().take(8).collect::<Vec<_>>().join("\n");
            format!("  ✗ build check failed:\n{excerpt}")
        }
        Err(e) => format!("  ⚠ build check could not run: {e}"),
    }
}

/// Print tool output truncated to the configured line limit.
/// The model always receives the full content regardless.
fn print_tool_output(output: &str, max_lines: usize, color: bool) {
    if output.is_empty() {
        return;
    }
    let max = max_lines;
    let lines: Vec<&str> = output.lines().collect();
    let shown = if max == 0 {
        lines.len()
    } else {
        lines.len().min(max)
    };
    let hidden = lines.len().saturating_sub(shown);

    let display = lines[..shown].join("\n");

    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::DarkGrey),
            Print(format!("{display}\n")),
            ResetColor,
        )
        .ok();
    } else {
        println!("{display}");
    }

    if hidden > 0 {
        // Just print the count and keep going — no blocking prompt.
        // The user can scroll back; the model always gets the full content.
        if color {
            execute!(
                io::stdout(),
                SetForegroundColor(CtColor::DarkGrey),
                Print(format!("  … ({hidden} more lines hidden)\n")),
                ResetColor,
            )
            .ok();
        } else {
            println!("  … ({hidden} more lines hidden)");
        }
    }
    io::stdout().flush().ok();
}

/// Whether a confined-shell envelope carries the STRUCTURED `denied: true`
/// flag — the leash's machine-readable signal that the brush interceptor
/// refused an exec / open inside the free-form command. Reads the structured
/// field agent-bridle emits; it does NOT parse stdout/stderr (the old stderr
/// string-match was fragile — a command that merely *printed* a denial-like
/// phrase could be misread, and any wording drift would silently break
/// detection).
fn envelope_denied(envelope: &serde_json::Value) -> bool {
    envelope
        .get("denied")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Build a human-readable denial message from the envelope's structured
/// `denials: [{ kind, target, reason }]` list, joining each entry's `reason`.
/// Falls back to a generic message when the list is missing or empty.
fn envelope_denial_reason(envelope: &serde_json::Value) -> String {
    let reasons: Vec<String> = envelope
        .get("denials")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|d| d.get("reason").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if reasons.is_empty() {
        "denied: the capability leash refused an operation".to_string()
    } else {
        reasons.join("; ")
    }
}

fn toml_string_literal(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn exec_allowlist_name(target: &str) -> &str {
    target
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(target)
}

fn extra_exec_hint(envelope: &serde_json::Value) -> Option<String> {
    let denials = envelope.get("denials")?.as_array()?;
    let target = denials.iter().find_map(|d| {
        let kind = d.get("kind")?.as_str()?;
        if kind != "exec" {
            return None;
        }
        d.get("target")?
            .as_str()
            .filter(|target| !target.is_empty())
    })?;

    Some(format!(
        "add it via [tui.permissions] extra_exec = [\"{}\"] in your newt config",
        toml_string_literal(exec_allowlist_name(target))
    ))
}

fn envelope_denial_reason_with_guidance(envelope: &serde_json::Value) -> String {
    let reason = envelope_denial_reason(envelope);
    match extra_exec_hint(envelope) {
        Some(hint) => format!("{reason} - {hint}"),
        None => reason,
    }
}

/// Execute a single tool call and return the result string sent back to the model.
///
/// `run_command` is routed through agent-bridle's Caveats-confined, brush-backed
/// `shell` tool: the WHOLE command runs inside the leash (`echo ok && rm -rf /`
/// no longer slips `rm` past an `echo` grant — every external spawn passes the
/// interceptor's `before_exec` / `before_open` gate). The fs tools
/// (`read_file` / `write_file` / `list_dir`) keep enforcing the same `caveats`
/// via `permits_*` — rerouting them is out of scope.
#[allow(clippy::too_many_arguments)]
async fn execute_tool(
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &newt_core::caveats::Caveats,
    mcp: &mut Mcp,
    build_check_cmd: Option<&str>,
) -> String {
    // Remote MCP tools (namespaced `server__tool`) route to their server before
    // the built-in match. They carry no Caveats leash in this build.
    if mcp.handles(name) {
        print_tool_call(name, &args.to_string(), color);
        let out = mcp.call(name, args).await;
        print_tool_output(&out, tool_output_lines, color);
        return out;
    }

    match name {
        "run_command" => {
            let cmd = args["command"].as_str().unwrap_or("");

            // Corrective guard: the model tried to call a tool as a shell binary.
            // Return a correction so the model can retry with the right tool call.
            if let Some(tool) = DIRECT_TOOL_NAMES
                .iter()
                .copied()
                .find(|t| cmd.split_ascii_whitespace().next() == Some(*t))
            {
                return format!(
                    "error: '{tool}' is a tool, not a shell command. \
                     Call it as a separate tool invocation — \
                     do not pass '{tool}' as a command argument to run_command."
                );
            }

            print_tool_call("run_command", cmd, color);

            // Route the WHOLE command through agent-bridle's confined shell
            // (free-form `cmd` mode) under the SAME Caveats the TUI resolved
            // from `[tui].permissions`. `caveats` is `newt_core::caveats::Caveats`,
            // a re-export of `agent_mesh_protocol::caveats::Caveats` — the exact
            // type `Registry::dispatch` expects, so no conversion is needed.
            //
            // Inject venv env vars if active: the confined shell does not inherit
            // the host environment, so we prepend export statements to the cmd.
            let cmd_with_venv = match venv_cmd_prefix() {
                Some(prefix) => format!("{prefix}{cmd}"),
                None => cmd.to_string(),
            };
            let dispatch_args = serde_json::json!({
                "cmd": cmd_with_venv,
                "cwd": workspace,
            });
            match agent_bridle::registry()
                .dispatch("shell", dispatch_args, caveats)
                .await
            {
                // The confined shell ran. Its envelope carries
                // `{ exit_code, stdout, stderr, timed_out, ... }` plus — when the
                // leash refused a capability — the STRUCTURED denial fields
                // `{ denied: true, denials: [{ kind, target, reason }] }`. In
                // free-form mode an out-of-scope command is denied *inside* the
                // shell by the brush interceptor (the command genuinely does not
                // run); we lift that to the existing capability-denied UX by
                // reading the structured `denied` field — NEVER a stderr grep.
                Ok(envelope) if envelope_denied(&envelope) => {
                    let reason = envelope_denial_reason_with_guidance(&envelope);
                    print_denied("exec", &reason, color);
                    format!("capability denied: {reason}")
                }
                Ok(envelope) => {
                    let stdout = envelope
                        .get("stdout")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    let stderr = envelope
                        .get("stderr")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    let out = format!("{stdout}{stderr}");
                    print_tool_output(&out, tool_output_lines, color);
                    if out.trim().is_empty() {
                        let code = envelope
                            .get("exit_code")
                            .and_then(serde_json::Value::as_i64)
                            .unwrap_or(-1);
                        format!("(exit {code})")
                    } else {
                        out
                    }
                }
                // An argv-mode leash denial, or an error from inside the tool —
                // surface the reason; the dispatch error Display is safe to show.
                Err(e) => format!("error: {e}"),
            }
        }

        "read_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let full = std::path::Path::new(workspace).join(path);
            let full_str = full.to_string_lossy();
            if !tui_permits_path(&caveats.fs_read, &full_str) {
                let msg = format!("capability denied: fs_read does not permit '{path}'");
                print_denied("fs_read", path, color);
                return msg;
            }
            print_tool_call("read_file", path, color);
            match std::fs::read_to_string(&full) {
                Ok(contents) => {
                    print_tool_output(&contents, tool_output_lines, color);
                    contents
                }
                Err(e) => format!("error reading {path}: {e}"),
            }
        }

        "write_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let content = args["content"].as_str().unwrap_or("");
            let full = std::path::Path::new(workspace).join(path);
            let full_str = full.to_string_lossy();
            if !tui_permits_path(&caveats.fs_write, &full_str) {
                let msg = format!("capability denied: fs_write does not permit '{path}'");
                print_denied("fs_write", path, color);
                return msg;
            }

            // Shrink guard: refuse if the proposed write removes > 30% of
            // lines AND > 30 lines absolute. This catches the failure mode
            // where a model replaces an entire large file with a small
            // fragment (observed in the wild: 4,247 → 107 lines).
            if let Ok(existing) = std::fs::read_to_string(&full) {
                let orig_lines = existing.lines().count();
                let new_lines = content.lines().count();
                let removed = orig_lines.saturating_sub(new_lines);
                if removed > 30 && new_lines < orig_lines * 7 / 10 {
                    let pct = removed * 100 / orig_lines.max(1);
                    let msg = format!(
                        "error: write_file would shrink {path} from {orig_lines} → {new_lines} lines \
                         (-{pct}%). This is likely unintentional. Use edit_file to make targeted \
                         changes, or ensure your content includes the full file."
                    );
                    print_denied("shrink-guard", path, color);
                    return msg;
                }
            }

            print_tool_call(
                "write_file",
                &format!("{path} ({} bytes)", content.len()),
                color,
            );

            // Show first 20 lines as preview.
            let preview: String = content.lines().take(20).collect::<Vec<_>>().join("\n");
            let has_more = content.lines().count() > 20;
            print_tool_output(
                &format!("{preview}{}", if has_more { "\n…" } else { "" }),
                tool_output_lines,
                color,
            );

            // Auto-write when the caveat explicitly scopes fs_write (the
            // preset itself is the user's consent).  Ask y/N only under
            // full_access / custom where fs_write == Scope::All.
            let needs_confirm = matches!(caveats.fs_write, newt_core::caveats::Scope::All);

            let confirmed = if needs_confirm {
                print!("Write this file? [y/N] ");
                io::stdout().flush().ok();
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer).is_ok()
                    && answer.trim().eq_ignore_ascii_case("y")
            } else {
                true
            };

            if confirmed {
                let full = std::path::Path::new(workspace).join(path);
                if let Some(parent) = full.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(&full, content) {
                    Ok(_) => {
                        let line_count = content.lines().count();
                        println!("✓ wrote {path} ({line_count} lines)");
                        let check = build_check_cmd
                            .map(|cmd| run_build_check(cmd, workspace))
                            .unwrap_or_default();
                        format!("wrote {path} ({line_count} lines){check}")
                    }
                    Err(e) => format!("error writing {path}: {e}"),
                }
            } else {
                println!("skipped");
                format!("user declined to write {path}")
            }
        }

        "edit_file" => {
            let path = args["path"].as_str().unwrap_or("");
            let old_string = args["old_string"].as_str().unwrap_or("");
            let new_string = args["new_string"].as_str().unwrap_or("");
            let full = std::path::Path::new(workspace).join(path);
            let full_str = full.to_string_lossy();
            if !tui_permits_path(&caveats.fs_write, &full_str) {
                let msg = format!("capability denied: fs_write does not permit '{path}'");
                print_denied("fs_write", path, color);
                return msg;
            }
            if old_string.is_empty() {
                return "error: old_string must not be empty — use write_file to create new files"
                    .to_string();
            }
            let existing = match std::fs::read_to_string(&full) {
                Ok(s) => s,
                Err(e) => return format!("error reading {path}: {e}"),
            };
            let count = existing.matches(old_string).count();
            if count == 0 {
                return format!(
                    "error: old_string not found in {path}. \
                     Check for whitespace differences or read the file first to confirm the exact text."
                );
            }
            if count > 1 {
                return format!(
                    "error: old_string matches {count} locations in {path}. \
                     Add more surrounding context to make it unique."
                );
            }
            let updated = existing.replacen(old_string, new_string, 1);
            let old_lines = existing.lines().count();
            let new_lines = updated.lines().count();
            let delta = new_lines as i64 - old_lines as i64;
            let delta_str = if delta >= 0 {
                format!("+{delta}")
            } else {
                format!("{delta}")
            };
            print_tool_call("edit_file", &format!("{path} ({delta_str} lines)"), color);
            match std::fs::write(&full, &updated) {
                Ok(_) => {
                    println!("✓ edited {path} ({delta_str} lines, now {new_lines} total)");
                    let check = build_check_cmd
                        .map(|cmd| run_build_check(cmd, workspace))
                        .unwrap_or_default();
                    format!("edited {path} ({delta_str} lines, now {new_lines} total){check}")
                }
                Err(e) => format!("error writing {path}: {e}"),
            }
        }

        "list_dir" => {
            let path = args["path"].as_str().unwrap_or(".");
            let full = std::path::Path::new(workspace).join(path);
            let full_str = full.to_string_lossy();
            if !tui_permits_path(&caveats.fs_read, &full_str) {
                let msg = format!("capability denied: fs_read does not permit '{path}'");
                print_denied("fs_read", path, color);
                return msg;
            }
            print_tool_call("list_dir", path, color);
            match std::fs::read_dir(&full) {
                Ok(entries) => {
                    let mut names: Vec<String> = entries
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect();
                    names.sort();
                    let listing = names.join("\n");
                    print_tool_output(&listing, tool_output_lines, color);
                    listing
                }
                Err(e) => format!("error: {e}"),
            }
        }

        "use_skill" => {
            let skill_name = args["name"].as_str().unwrap_or("");
            print_tool_call("use_skill", skill_name, color);
            // Reads from the configured skill search path. This is a read of
            // trusted operator config (procedural knowledge), not an exec of
            // arbitrary code, so it is NOT leash-gated — any SCRIPTS the skill
            // bundles still run through `run_command`'s confined shell and are
            // governed by the session caveats. The same first-directory-wins
            // precedence as the index means we load the copy the model was
            // actually shown.
            let dirs = newt_core::Config::resolve()
                .map(|c| c.skill_search_dirs())
                .unwrap_or_default();
            match newt_skills::load_body_from(&dirs, skill_name) {
                Ok(body) => {
                    print_tool_output(&body, tool_output_lines, color);
                    body
                }
                Err(e) => format!("error: {e}"),
            }
        }

        "web_fetch" => {
            let url = args["url"].as_str().unwrap_or("");
            print_tool_call("web_fetch", url, color);

            // Route through agent-bridle's `web_fetch` tool under the SAME
            // Caveats. The `net` axis gates which hosts are reachable (host
            // allowlist + SSRF screen); an out-of-scope host is denied by the
            // leash, surfaced via the dispatch error. The tool returns extracted
            // markdown (`{ url, final_url, status, title, markdown }`) — the body
            // is untrusted page content, not a command result.
            let mut fetch_args = serde_json::json!({ "url": url });
            if let Some(max_bytes) = args.get("max_bytes").and_then(serde_json::Value::as_u64) {
                fetch_args["max_bytes"] = serde_json::json!(max_bytes);
            }
            match agent_bridle::registry()
                .dispatch("web_fetch", fetch_args, caveats)
                .await
            {
                Ok(result) => {
                    let markdown = result
                        .get("markdown")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    let title = result
                        .get("title")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    let final_url = result
                        .get("final_url")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or(url);
                    let out = if title.is_empty() {
                        format!("{final_url}\n\n{markdown}")
                    } else {
                        format!("# {title}\n{final_url}\n\n{markdown}")
                    };
                    print_tool_output(&out, tool_output_lines, color);
                    out
                }
                // A `net`-axis leash denial, or a fetch error (SSRF screen,
                // timeout, non-2xx) — surface the reason; Display is safe.
                Err(e) => format!("error: {e}"),
            }
        }

        other => format!("unknown tool: {other}"),
    }
}

struct ChatCtx<'a> {
    url: &'a str,
    model: &'a str,
    /// Wire protocol of the active backend (Ollama vs OpenAI-compatible).
    kind: newt_core::BackendKind,
    /// Bearer token for authenticated OpenAI-compatible endpoints.
    api_key: Option<&'a str>,
    /// Full message list already assembled by `MemoryManager::build_messages`.
    messages: &'a [newt_core::MemMessage],
    task: &'a str,
    workspace: &'a str,
    color: bool,
    caveats: &'a newt_core::caveats::Caveats,
    /// Maximum tool-call rounds before forcing a final tools-disabled
    /// completion (from `[tui].max_tool_rounds`, default 25).
    max_tool_rounds: usize,
    /// Max lines of tool output shown inline (from `[tui].tool_output_lines`,
    /// default 20). Resolved once per turn and threaded to `execute_tool` so
    /// the tool loop never re-reads config from disk.
    tool_output_lines: usize,
    /// Enable per-round diagnostic output. Set via `NEWT_DEBUG=1` or the
    /// `[tui] debug = true` config key.
    debug: bool,
    /// Ollama `options.num_ctx` — caps KV-cache allocation to prevent VRAM
    /// exhaustion on large models. `None` → model default (often 131k).
    num_ctx: Option<u32>,
    /// TCP connect timeout. Short (5 s default) so a down endpoint fails fast
    /// rather than blocking the full `inference_timeout_secs`.
    connect_timeout_secs: u64,
    /// Total inference timeout. Must be long enough for the model to generate
    /// a complete response (120 s default).
    inference_timeout_secs: u64,
    /// Message list size at which the agent trims the middle of the in-flight
    /// conversation to prevent context overflow mid-turn.
    mid_loop_trim_threshold: usize,
    /// Shell command run after every successful file write to give the model
    /// immediate ground-truth feedback (e.g. "cargo check -q --workspace").
    /// `None` disables auto-checking. Set per-workspace in `.newt/config.toml`.
    build_check_cmd: Option<String>,
}

/// Main agentic loop: call model → execute tool calls → feed results back → repeat.
/// Returns `(reply_text, was_streamed, token_usage, hallucination_count)`.
/// When `was_streamed` is true the text was already printed token-by-token.
async fn chat_complete(
    ctx: ChatCtx<'_>,
    mcp: &mut Mcp,
) -> anyhow::Result<(String, bool, Option<newt_core::TokenUsage>, u32)> {
    // OpenAI-compatible endpoints speak a different wire format (request,
    // tool_calls, and usage shapes all differ), so they get their own loop.
    if ctx.kind == newt_core::BackendKind::Openai {
        return openai_chat_complete(ctx, mcp).await;
    }
    let ChatCtx {
        url,
        model,
        kind: _,
        api_key: _,
        messages: mem_messages,
        task: _task,
        workspace,
        color,
        caveats,
        max_tool_rounds,
        tool_output_lines,
        debug,
        num_ctx,
        connect_timeout_secs,
        inference_timeout_secs,
        mid_loop_trim_threshold,
        build_check_cmd,
    } = ctx;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs))
        .timeout(std::time::Duration::from_secs(inference_timeout_secs))
        .build()?;
    let chat_url = format!("{}/api/chat", url.trim_end_matches('/'));
    let retry = tui_retry_policy();

    // Convert MemMessage list to Ollama JSON format.
    // The memory manager already included the current task as the last user message.
    let mut messages: Vec<serde_json::Value> = mem_messages
        .iter()
        .map(|m| serde_json::json!({"role": m.role.as_str(), "content": m.content}))
        .collect();

    let mut accumulated_usage: Option<newt_core::TokenUsage> = None;
    let mut hallucination_count: u32 = 0;

    // Agentic loop — up to `max_tool_rounds` tool-call rounds.
    for round in 0..max_tool_rounds {
        if round > 0 {
            // Brief separator between rounds so user can follow the flow.
            if color {
                execute!(
                    io::stdout(),
                    SetForegroundColor(CtColor::DarkGrey),
                    Print("…\n"),
                    ResetColor
                )
                .ok();
            }
        }

        // Mid-loop context trim: prevent VRAM exhaustion on long tool-call
        // sessions by dropping old middle messages when the list grows large.
        if messages.len() > mid_loop_trim_threshold {
            let before = messages.len();
            messages = trim_for_summary(&messages, 2, mid_loop_trim_threshold / 2);
            if debug {
                print_debug(
                    &format!(
                        "mid-loop trim: {before} → {} messages (threshold={})",
                        messages.len(),
                        mid_loop_trim_threshold
                    ),
                    color,
                );
            }
        }

        // Tool-call rounds: stream:false (fast, just JSON).
        // Final text round: stream:true so the user sees tokens arrive.
        // We don't know which round is last, so we probe with stream:false first
        // and switch to streaming only when the model returns no tool calls.
        let body_no_stream = if let Some(ctx_size) = num_ctx {
            serde_json::json!({
                "model": model,
                "messages": messages,
                "stream": false,
                "tools": merged_tool_definitions(mcp),
                "options": { "num_ctx": ctx_size },
            })
        } else {
            serde_json::json!({
                "model": model,
                "messages": messages,
                "stream": false,
                "tools": merged_tool_definitions(mcp),
            })
        };

        // Retry the send+status+parse as one unit — a connection drop at any
        // of these steps is transient and worth retrying with backoff.
        let json: serde_json::Value = with_backoff_notify(
            &retry,
            || async {
                let resp = client
                    .post(&chat_url)
                    .json(&body_no_stream)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("Ollama {status}: {text}");
                }
                resp.json::<serde_json::Value>()
                    .await
                    .map_err(anyhow::Error::from)
            },
            |attempt, delay| print_retry_indicator(attempt, delay, color),
        )
        .await?;

        // Accumulate token usage from this non-streaming probe round.
        let round_usage = ollama_usage(&json);
        accumulated_usage = merge_usage(accumulated_usage, round_usage);

        let message = &json["message"];
        // Capture the probe content now — it may be our only copy of the
        // model's reply if the subsequent streaming re-issue returns empty.
        let probe_content = message["content"].as_str().unwrap_or("").to_string();

        let tool_calls = message["tool_calls"].as_array();
        let has_tools = tool_calls.map(|tc| !tc.is_empty()).unwrap_or(false);

        if debug {
            let content_excerpt = if probe_content.is_empty() {
                "(empty)".to_string()
            } else {
                let chars: String = probe_content.chars().take(80).collect();
                if probe_content.len() > 80 {
                    format!("{chars}…")
                } else {
                    chars
                }
            };
            let tc_count = tool_calls.map(|tc| tc.len()).unwrap_or(0);
            let usage_str = match round_usage {
                Some(u) => format!("{} in / {} out", u.input_tokens, u.output_tokens),
                None => "no usage".into(),
            };
            print_debug(
                &format!(
                    "round {round} probe: tool_calls={tc_count} usage=[{usage_str}] content={content_excerpt:?}"
                ),
                color,
            );
        }

        if !has_tools {
            // No tool calls — re-issue with stream:true so the user sees tokens.
            // `messages` already contains the task; just replay with streaming.
            //
            // IMPORTANT: the probe round already generated the model's answer in
            // `probe_content`. The streaming re-issue is a *second* inference call
            // from the same history; if it returns empty (non-determinism, context
            // pressure, or model quirk) we fall back to the probe content so the
            // user never sees a silent blank response.
            let body_stream = if let Some(ctx_size) = num_ctx {
                serde_json::json!({
                    "model": model,
                    "messages": &messages,
                    "stream": true,
                    "tools": merged_tool_definitions(mcp),
                    "options": { "num_ctx": ctx_size },
                })
            } else {
                serde_json::json!({
                    "model": model,
                    "messages": &messages,
                    "stream": true,
                    "tools": merged_tool_definitions(mcp),
                })
            };
            // Retry the connection; if we connect successfully but the stream
            // drops mid-token, that's a separate (harder) failure mode.
            let sresp = with_backoff_notify(
                &retry,
                || async {
                    client
                        .post(&chat_url)
                        .json(&body_stream)
                        .send()
                        .await
                        .map_err(|e| anyhow::anyhow!("stream request failed: {e}"))
                },
                |attempt, delay| print_retry_indicator(attempt, delay, color),
            )
            .await?;

            if !sresp.status().is_success() {
                if debug {
                    print_debug("stream request non-2xx — using probe content", color);
                }
                return Ok((probe_content, false, accumulated_usage, hallucination_count));
            }
            let (streamed, stream_usage) = stream_response(sresp, color).await?;

            if streamed.is_empty() {
                // The streaming re-issue produced no tokens. Fall back to the
                // probe content rather than returning silence.
                if debug {
                    print_debug(
                        &format!(
                            "stream returned empty — falling back to probe content ({} chars)",
                            probe_content.len()
                        ),
                        color,
                    );
                }
                if probe_content.is_empty() {
                    // Both probe and stream are empty — the model produced nothing.
                    let msg = "(model returned an empty response — try rephrasing, or check the model with `newt doctor`)";
                    return Ok((
                        msg.to_string(),
                        false,
                        merge_usage(accumulated_usage, stream_usage),
                        hallucination_count,
                    ));
                }
                // Use probe content; print it since it was never streamed.
                return Ok((
                    probe_content,
                    false,
                    merge_usage(accumulated_usage, stream_usage),
                    hallucination_count,
                ));
            }

            return Ok((
                streamed,
                true,
                merge_usage(accumulated_usage, stream_usage),
                hallucination_count,
            ));
        }

        // Has tool calls — add assistant turn and execute them.
        messages.push(message.clone());
        for tc in tool_calls.unwrap() {
            let name = tc["function"]["name"].as_str().unwrap_or("unknown");
            let args = match &tc["function"]["arguments"] {
                serde_json::Value::String(s) => {
                    serde_json::from_str(s).unwrap_or(serde_json::Value::Null)
                }
                v => v.clone(),
            };
            if is_hallucination(name, &args) {
                hallucination_count += 1;
            }
            let result = execute_tool(
                name,
                &args,
                workspace,
                color,
                tool_output_lines,
                caveats,
                mcp,
                build_check_cmd.as_deref(),
            )
            .await;
            messages.push(serde_json::json!({
                "role": "tool",
                "content": result
            }));
        }
    }

    // Reached the round cap. Trim the bloated message list so the final
    // summary request doesn't overflow the model's context window, then
    // make ONE tools-disabled completion so the user gets a real partial answer.
    let trimmed = trim_for_summary(&messages, 2, 6);
    let (text, streamed, usage) = final_summary_ollama(
        &client,
        &chat_url,
        model,
        trimmed,
        max_tool_rounds,
        accumulated_usage,
    )
    .await?;
    Ok((text, streamed, usage, hallucination_count))
}

/// Build the nudge appended to the message list when the tool-round cap is hit.
fn cap_exit_nudge(max_tool_rounds: usize) -> String {
    format!(
        "You have reached the tool-call limit ({max_tool_rounds} rounds). \
         Do NOT call any more tools. Summarize what you found across the tool \
         calls above and give your best final answer now."
    )
}

/// Fallback message returned when even the final tools-disabled completion
/// fails. Includes accumulated token counts so the user knows what was consumed,
/// and gives actionable advice rather than just naming the limit.
fn cap_exit_fallback(max_tool_rounds: usize, accumulated: Option<newt_core::TokenUsage>) -> String {
    let tokens_hint = match accumulated {
        Some(u) => format!(
            " ({} in / {} out tokens consumed across {max_tool_rounds} rounds)",
            u.input_tokens, u.output_tokens,
        ),
        None => String::new(),
    };
    format!(
        "(reached the tool-call limit of {max_tool_rounds} rounds{tokens_hint}, \
         and the final summarization request also failed — \
         raise [tui].max_tool_rounds in your config, or ask a more focused question)"
    )
}

/// Final tools-disabled completion for the Ollama (`/api/chat`) path.
///
/// `messages` is the already-trimmed list (caller uses `trim_for_summary`).
/// `accumulated` carries usage from the preceding tool-call rounds so it
/// survives even when this summary request fails.
async fn final_summary_ollama(
    client: &reqwest::Client,
    chat_url: &str,
    model: &str,
    mut messages: Vec<serde_json::Value>,
    max_tool_rounds: usize,
    accumulated: Option<newt_core::TokenUsage>,
) -> anyhow::Result<(String, bool, Option<newt_core::TokenUsage>)> {
    messages.push(serde_json::json!({
        "role": "user",
        "content": cap_exit_nudge(max_tool_rounds),
    }));
    // No `tools` key => the model cannot emit tool calls.
    let body = serde_json::json!({
        "model": model,
        "messages": &messages,
        "stream": false,
    });
    let retry = tui_retry_policy();
    let result = with_backoff_notify(
        &retry,
        || async {
            let resp = client
                .post(chat_url)
                .json(&body)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("Ollama {status}: {text}");
            }
            resp.json::<serde_json::Value>()
                .await
                .map_err(anyhow::Error::from)
        },
        |_, _| {}, // no color context here; tracing::warn covers it
    )
    .await;
    match result {
        Ok(json) => {
            let content = json["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let total = merge_usage(accumulated, ollama_usage(&json));
            if content.is_empty() {
                Ok((
                    cap_exit_fallback(max_tool_rounds, accumulated),
                    false,
                    accumulated,
                ))
            } else {
                Ok((content, false, total))
            }
        }
        // On any failure (including exhausted retries), still return the
        // accumulated usage so the caller can log the tokens consumed.
        Err(_) => Ok((
            cap_exit_fallback(max_tool_rounds, accumulated),
            false,
            accumulated,
        )),
    }
}

/// Final tools-disabled completion for the OpenAI (`/v1/chat/completions`) path.
///
/// `messages` is the already-trimmed list (caller uses `trim_for_summary`).
/// `accumulated` carries usage from the preceding tool-call rounds.
async fn final_summary_openai(
    client: &reqwest::Client,
    chat_url: &str,
    model: &str,
    api_key: Option<&str>,
    mut messages: Vec<serde_json::Value>,
    max_tool_rounds: usize,
    accumulated: Option<newt_core::TokenUsage>,
) -> anyhow::Result<(String, bool, Option<newt_core::TokenUsage>)> {
    messages.push(serde_json::json!({
        "role": "user",
        "content": cap_exit_nudge(max_tool_rounds),
    }));
    // Omit `tools` / `tool_choice` => the model cannot emit tool calls.
    let body = serde_json::json!({
        "model": model,
        "messages": &messages,
        "stream": false,
    });
    let retry = tui_retry_policy();
    let result = with_backoff_notify(
        &retry,
        || async {
            let mut req = client.post(chat_url).json(&body);
            if let Some(key) = api_key {
                req = req.bearer_auth(key);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("inference endpoint {status}: {text}");
            }
            resp.json::<serde_json::Value>()
                .await
                .map_err(anyhow::Error::from)
        },
        |_, _| {},
    )
    .await;
    match result {
        Ok(json) => {
            let content = json["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let total = merge_usage(accumulated, openai_usage(&json["usage"]));
            if content.is_empty() {
                Ok((
                    cap_exit_fallback(max_tool_rounds, accumulated),
                    false,
                    accumulated,
                ))
            } else {
                Ok((content, false, total))
            }
        }
        Err(_) => Ok((
            cap_exit_fallback(max_tool_rounds, accumulated),
            false,
            accumulated,
        )),
    }
}

/// OpenAI-compatible variant of [`chat_complete`]: the same agentic tool-call
/// loop, but over `POST {endpoint}/v1/chat/completions` with bearer auth and
/// the OpenAI `tool_calls` / `tool_call_id` / `usage` shapes.
///
/// Non-streaming for now — the final answer is returned (and printed by the
/// caller) rather than streamed token-by-token. Token-by-token SSE streaming
/// is a follow-up; functionally the loop is complete, including tools.
async fn openai_chat_complete(
    ctx: ChatCtx<'_>,
    mcp: &mut Mcp,
) -> anyhow::Result<(String, bool, Option<newt_core::TokenUsage>, u32)> {
    let ChatCtx {
        url,
        model,
        kind: _,
        api_key,
        messages: mem_messages,
        task: _task,
        workspace,
        color,
        caveats,
        max_tool_rounds,
        tool_output_lines,
        debug,
        num_ctx,
        connect_timeout_secs,
        inference_timeout_secs,
        mid_loop_trim_threshold,
        build_check_cmd,
    } = ctx;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs))
        .timeout(std::time::Duration::from_secs(inference_timeout_secs))
        .build()?;
    let chat_url = format!("{}/v1/chat/completions", url.trim_end_matches('/'));
    let retry = tui_retry_policy();

    let mut messages: Vec<serde_json::Value> = mem_messages
        .iter()
        .map(|m| serde_json::json!({"role": m.role.as_str(), "content": m.content}))
        .collect();

    let mut accumulated_usage: Option<newt_core::TokenUsage> = None;
    let mut hallucination_count: u32 = 0;

    // Agentic loop — up to `max_tool_rounds` tool-call rounds (matches the Ollama path).
    for round in 0..max_tool_rounds {
        if round > 0 && color {
            execute!(
                io::stdout(),
                SetForegroundColor(CtColor::DarkGrey),
                Print("…\n"),
                ResetColor
            )
            .ok();
        }

        // Mid-loop context trim (mirrors Ollama path).
        if messages.len() > mid_loop_trim_threshold {
            let before = messages.len();
            messages = trim_for_summary(&messages, 2, mid_loop_trim_threshold / 2);
            if debug {
                print_debug(
                    &format!(
                        "mid-loop trim: {before} → {} messages (threshold={})",
                        messages.len(),
                        mid_loop_trim_threshold
                    ),
                    color,
                );
            }
        }

        // OpenAI-compatible endpoints don't use Ollama's `options.num_ctx` —
        // context limits are configured server-side (vLLM --max-model-len).
        let body = serde_json::json!({
            "model": model,
            "messages": messages,
            "tools": merged_tool_definitions(mcp),
            "tool_choice": "auto",
            "stream": false,
        });
        let _ = num_ctx; // not applicable for OpenAI-compatible endpoints
        let json: serde_json::Value = with_backoff_notify(
            &retry,
            || async {
                let mut req = client.post(&chat_url).json(&body);
                if let Some(key) = api_key {
                    req = req.bearer_auth(key);
                }
                let resp = req
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("inference endpoint {status}: {text}");
                }
                resp.json::<serde_json::Value>()
                    .await
                    .map_err(anyhow::Error::from)
            },
            |attempt, delay| print_retry_indicator(attempt, delay, color),
        )
        .await?;
        // Accumulate per-round token usage.
        let round_usage = openai_usage(&json["usage"]);
        accumulated_usage = merge_usage(accumulated_usage, round_usage);

        let message = &json["choices"][0]["message"];

        let tool_calls = message["tool_calls"].as_array();
        let has_tools = tool_calls.map(|tc| !tc.is_empty()).unwrap_or(false);

        if debug {
            let content = message["content"].as_str().unwrap_or("");
            let excerpt: String = content.chars().take(80).collect();
            let tc_count = tool_calls.map(|tc| tc.len()).unwrap_or(0);
            let usage_str = match round_usage {
                Some(u) => format!("{} in / {} out", u.input_tokens, u.output_tokens),
                None => "no usage".into(),
            };
            print_debug(
                &format!(
                    "round {round}: tool_calls={tc_count} usage=[{usage_str}] content={excerpt:?}"
                ),
                color,
            );
        }

        if !has_tools {
            let content = message["content"].as_str().unwrap_or("").to_string();
            if content.is_empty() && debug {
                print_debug(
                    "empty content with no tool calls — model produced nothing",
                    color,
                );
            }
            let out = if content.is_empty() {
                "(model returned an empty response — try rephrasing, or check the model with `newt doctor`)".to_string()
            } else {
                content
            };
            return Ok((out, false, accumulated_usage, hallucination_count));
        }

        // Record the assistant turn verbatim (it carries the tool_calls), then
        // run each call and feed the result back keyed by its tool_call_id.
        messages.push(message.clone());
        for tc in tool_calls.unwrap() {
            let id = tc["id"].as_str().unwrap_or("");
            let name = tc["function"]["name"].as_str().unwrap_or("unknown");
            let args = match &tc["function"]["arguments"] {
                serde_json::Value::String(s) => {
                    serde_json::from_str(s).unwrap_or(serde_json::Value::Null)
                }
                v => v.clone(),
            };
            if is_hallucination(name, &args) {
                hallucination_count += 1;
            }
            let result = execute_tool(
                name,
                &args,
                workspace,
                color,
                tool_output_lines,
                caveats,
                mcp,
                build_check_cmd.as_deref(),
            )
            .await;
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": id,
                "content": result,
            }));
        }
    }

    // Reached the round cap. Trim the message list and make ONE final
    // tools-disabled completion (matches the Ollama path).
    let trimmed = trim_for_summary(&messages, 2, 6);
    let (text, streamed, usage) = final_summary_openai(
        &client,
        &chat_url,
        model,
        api_key,
        trimmed,
        max_tool_rounds,
        accumulated_usage,
    )
    .await?;
    Ok((text, streamed, usage, hallucination_count))
}

/// Parse an OpenAI `usage` object (`prompt_tokens` / `completion_tokens`).
fn openai_usage(usage: &serde_json::Value) -> Option<newt_core::TokenUsage> {
    let input = usage["prompt_tokens"].as_u64()? as u32;
    let output = usage["completion_tokens"].as_u64()? as u32;
    Some(newt_core::TokenUsage {
        input_tokens: input,
        output_tokens: output,
    })
}

/// Stream an Ollama NDJSON response, printing tokens as they arrive.
/// Returns `(accumulated_text, token_usage)`.
/// Token usage is extracted from the final chunk (`done: true`).
async fn stream_response(
    resp: reqwest::Response,
    color: bool,
) -> anyhow::Result<(String, Option<newt_core::TokenUsage>)> {
    let mut full = String::new();
    let mut started = false;
    let mut usage: Option<newt_core::TokenUsage> = None;

    let mut resp = resp;
    while let Some(chunk) = resp.chunk().await? {
        let text = String::from_utf8_lossy(&chunk);
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let token = json["message"]["content"].as_str().unwrap_or("");
            if !token.is_empty() {
                if !started {
                    if color {
                        execute!(
                            io::stdout(),
                            SetForegroundColor(NEWT_ORANGE_CT),
                            Print("▸  "),
                            ResetColor,
                        )
                        .ok();
                    } else {
                        print!("▸  ");
                    }
                    started = true;
                }
                print!("{token}");
                io::stdout().flush().ok();
                full.push_str(token);
            }
            if json["done"].as_bool().unwrap_or(false) {
                // Extract token counts from the final Ollama chunk.
                let input = json["prompt_eval_count"].as_u64().map(|n| n as u32);
                let output = json["eval_count"].as_u64().map(|n| n as u32);
                usage = input.zip(output).map(|(i, o)| newt_core::TokenUsage {
                    input_tokens: i,
                    output_tokens: o,
                });
                break;
            }
        }
    }
    if started {
        println!();
    }
    Ok((full, usage))
}

/// Print the telemetry summary line after an inference turn.
fn print_metrics(metrics: &newt_core::TurnMetrics, color: bool) {
    let line = metrics.display_line();
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::DarkGrey),
            Print(format!("  {line}\n")),
            ResetColor,
        )
        .ok();
    } else {
        println!("  {line}");
    }
    io::stdout().flush().ok();
}

fn print_thinking(color: bool) {
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::DarkGrey),
            Print("▸  thinking…"),
            ResetColor,
        )
        .ok();
        io::stdout().flush().ok();
    }
}

fn erase_line() {
    print!("\r{}\r", " ".repeat(20));
    io::stdout().flush().ok();
}

// ---------------------------------------------------------------------------
// Slash command dispatcher
// ---------------------------------------------------------------------------

/// Dispatch a `/command` line. Returns `true` to keep the session alive,
/// `false` to exit.
fn dispatch_slash(
    input: &str,
    workspace: &str,
    color: bool,
    verbose: bool,
) -> anyhow::Result<bool> {
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
                "  /models                  — list models on the active endpoint",
                "  /models capabilities     — tool-conformance matrix (cached)",
                "  /model <name>            — switch model for this session",
                "  /probe [model|all]       — test tool conformance and cache the result",
                "  /memory                  — show context window / notes usage",
                "  /remember <fact>         — add a fact to persistent NOTES.md",
                "  /new                     — start a fresh conversation",
                "  /persona list            — list configured personas",
                "  /persona show            — show the active persona",
                "  /persona <name>          — start fresh with a persona",
                "  /persona clear           — start fresh with no persona",
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

        "version" => print_newt(&format!("v{VERSION}"), color, verbose),

        "workspace" => print_newt(workspace, color, verbose),

        "models" => {
            let cfg = newt_core::Config::resolve().unwrap_or_default();
            let choice = resolve_backend_choice(&cfg);
            let url = choice.url;
            let current = choice.model;

            if arg1 == "capabilities" {
                // Full tool-conformance matrix from the capability cache.
                match probe::fetch_ollama_models(&url) {
                    Err(e) => print_newt(&format!("error: {e}"), color, verbose),
                    Ok(models) => {
                        let cache = probe::load_cache();
                        probe::print_capabilities_table(&models, &cache, &current, &url, color);
                    }
                }
            } else {
                // Plain list, with cached conformance symbol where known.
                let fetched = if choice.kind == newt_core::BackendKind::Openai {
                    fetch_openai_models(&url, choice.api_key.as_deref())
                } else {
                    fetch_models_from_url(&url)
                };
                match fetched {
                    Ok(names) if names.is_empty() => {
                        print_newt(&format!("No models found on {url}"), color, verbose);
                    }
                    Ok(names) => {
                        let cache = probe::load_cache();
                        print_newt(&format!("Models on {url}:"), color, verbose);
                        for name in &names {
                            let conformance_tag = cache
                                .get(name)
                                .map(|e| format!("  {}", e.conformance.symbol()))
                                .unwrap_or_default();
                            if *name == current {
                                if color {
                                    execute!(
                                        io::stdout(),
                                        Print(format!("  {name}{conformance_tag}")),
                                        SetForegroundColor(NEWT_ORANGE_CT),
                                        Print(" ◀ active"),
                                        ResetColor,
                                        Print("\n"),
                                    )
                                    .ok();
                                } else {
                                    println!("  {name}{conformance_tag} ◀ active");
                                }
                            } else {
                                println!("  {name}{conformance_tag}");
                            }
                        }
                        let tested = names.iter().filter(|n| cache.contains_key(*n)).count();
                        if tested < names.len() {
                            println!(
                                "\n  {}/{} tested — /models capabilities for the full matrix",
                                tested,
                                names.len()
                            );
                        }
                    }
                    Err(e) => print_newt(&format!("error: {e}"), color, verbose),
                }
            }
        }

        "probe" => {
            // Test tool conformance for one model (or all untested).
            let cfg = newt_core::Config::resolve().unwrap_or_default();
            let choice = resolve_backend_choice(&cfg);

            if choice.kind != newt_core::BackendKind::Ollama {
                print_newt(
                    "/probe only works with Ollama endpoints (vLLM/OpenAI keep models resident)",
                    color,
                    verbose,
                );
            } else {
                let endpoint = &choice.url;
                let mut cache = probe::load_cache();

                // Decide which models to probe.
                let targets: Vec<String> = if arg1 == "all" {
                    match probe::fetch_ollama_models(endpoint) {
                        Ok(models) => models
                            .into_iter()
                            .filter(|m| !cache.contains_key(&m.name))
                            .map(|m| m.name)
                            .collect(),
                        Err(e) => {
                            print_newt(&format!("error fetching model list: {e}"), color, verbose);
                            vec![]
                        }
                    }
                } else if arg1.is_empty() {
                    vec![choice.model.clone()]
                } else {
                    vec![arg1.to_string()]
                };

                if targets.is_empty() {
                    print_newt(
                        "All models already tested — use /probe <name> to re-test one.",
                        color,
                        verbose,
                    );
                }

                for model in &targets {
                    // Warm up before probing so load time doesn't count as a timeout.
                    print_newt(&format!("Probing {model}…"), color, verbose);
                    warmup_if_cold(endpoint, model, &keep_alive_str(&cfg), color, verbose);

                    let result = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current()
                            .block_on(probe::probe_tool_conformance(endpoint, model))
                    });
                    match result {
                        Ok(conformance) => {
                            let today = today_date();
                            let symbol = conformance.symbol();
                            print_newt(
                                &format!("{model}  →  {symbol}  (tested {today})"),
                                color,
                                verbose,
                            );
                            // Preserve existing context-window tuning if the
                            // model was already in the cache.
                            let existing = cache.remove(model.as_str()).unwrap_or_default();
                            cache.insert(
                                model.clone(),
                                probe::CapabilityEntry {
                                    conformance,
                                    tested_date: today,
                                    ..existing
                                },
                            );
                            probe::save_cache(&cache);
                        }
                        Err(e) => {
                            print_newt(&format!("{model}  →  error: {e}"), color, verbose);
                        }
                    }
                }
            }
        }

        "model" => {
            if arg1.is_empty() {
                let cfg = newt_core::Config::resolve().unwrap_or_default();
                let current = resolve_backend_choice(&cfg).model;
                print_newt(
                    &format!("active model: {current}  (use /model <name> to switch)"),
                    color,
                    verbose,
                );
            } else {
                // Persist via `newt dgx use <model>` then resolve_backend_config
                // picks it up automatically on the next turn.
                run_newt_subcmd(&["dgx", "use", arg1], color, verbose)?;
                // Re-resolve so the warm-up targets the new endpoint.
                let cfg = newt_core::Config::resolve().unwrap_or_default();
                let choice = resolve_backend_choice(&cfg);
                // Warm-up only applies to Ollama: vLLM and OpenAI-compatible
                // endpoints keep their served model resident at all times.
                if choice.kind == newt_core::BackendKind::Ollama {
                    warmup_if_cold(&choice.url, arg1, &keep_alive_str(&cfg), color, verbose);
                } else {
                    print_newt(
                        &format!("Switched to {arg1} — takes effect on next message."),
                        color,
                        verbose,
                    );
                }
            }
        }

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

        other => print_newt(
            &format!("unknown command: /{other}  (try /help)"),
            color,
            verbose,
        ),
    }
    Ok(true)
}

/// Fetch model names from an Ollama endpoint's `/api/tags`.
fn fetch_models_from_url(url: &str) -> anyhow::Result<Vec<String>> {
    let tags_url = format!("{}/api/tags", url.trim_end_matches('/'));
    let json: serde_json::Value = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let resp = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()?
                .get(&tags_url)
                .send()
                .await?;
            if !resp.status().is_success() {
                anyhow::bail!("HTTP {}", resp.status());
            }
            resp.json::<serde_json::Value>().await.map_err(Into::into)
        })
    })?;
    Ok(parse_model_names(&json))
}

/// Check whether `model` is currently loaded in Ollama's VRAM via `/api/ps`.
/// Returns `true` when the model is resident and ready; `false` when cold.
/// Silently returns `false` on any network or parse error so the caller always
/// falls through to the warm-up path — a false negative just means we warm
/// unnecessarily, which is safe.
fn is_model_resident(endpoint: &str, model: &str) -> bool {
    let ps_url = format!("{}/api/ps", endpoint.trim_end_matches('/'));
    let Ok(json) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let resp = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()?
                .get(&ps_url)
                .send()
                .await?;
            resp.json::<serde_json::Value>()
                .await
                .map_err(anyhow::Error::from)
        })
    }) else {
        return false;
    };
    json["models"]
        .as_array()
        .map(|arr| arr.iter().any(|m| m["name"].as_str() == Some(model)))
        .unwrap_or(false)
}

/// After a `/model <name>` switch, warm the new model if it isn't already
/// resident in Ollama's VRAM. Blocks until the model is loaded (or the
/// warm-up itself fails), then prints a ready line.
///
/// Skipped silently on non-Ollama endpoints (caller's responsibility).
fn warmup_if_cold(endpoint: &str, model: &str, keep_alive: &str, color: bool, verbose: bool) {
    if is_model_resident(endpoint, model) {
        print_newt(
            &format!("✓ {model} — already resident, ready"),
            color,
            verbose,
        );
        return;
    }

    // Print the warning BEFORE blocking so the user sees it immediately.
    let msg = format!("⏳ {model} is cold — warming up (large models can take 30–60 s)…");
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::Rgb {
                r: 200,
                g: 140,
                b: 0,
            }),
            Print(format!("▸  {msg}\n")),
            ResetColor,
        )
        .ok();
    } else {
        println!("▸  {msg}");
    }
    io::stdout().flush().ok();

    // Large models (70b Q8) can take 60+ seconds to load; use a generous
    // timeout and the same retry policy as the rest of the TUI.
    let warm_url = format!("{}/api/generate", endpoint.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "keep_alive": keep_alive,
        "stream": false,
    });
    let retry = tui_retry_policy();
    let result = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()?;
            with_backoff_notify(
                &retry,
                || async {
                    let resp = client
                        .post(&warm_url)
                        .json(&body)
                        .send()
                        .await
                        .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
                    if !resp.status().is_success() {
                        anyhow::bail!("Ollama {}", resp.status());
                    }
                    resp.json::<serde_json::Value>()
                        .await
                        .map_err(anyhow::Error::from)
                },
                |attempt, delay| print_retry_indicator(attempt, delay, color),
            )
            .await
        })
    });

    match result {
        Ok(json) => {
            let ready_msg = match json["load_duration"].as_u64() {
                Some(ns) if ns > 0 => {
                    format!("✓ {model} — loaded in {:.1}s, ready", ns as f64 / 1e9)
                }
                _ => format!("✓ {model} — ready"),
            };
            print_newt(&ready_msg, color, verbose);
        }
        Err(e) => print_newt(
            &format!("⚠ warm-up failed: {e} — first response may be slow"),
            color,
            verbose,
        ),
    }
}

/// Fetch model ids from an OpenAI-compatible endpoint's `/v1/models`, with
/// optional bearer auth.
fn fetch_openai_models(url: &str, api_key: Option<&str>) -> anyhow::Result<Vec<String>> {
    let models_url = format!("{}/v1/models", url.trim_end_matches('/'));
    let api_key = api_key.map(str::to_string);
    let json: serde_json::Value = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let mut req = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()?
                .get(&models_url);
            if let Some(key) = api_key {
                req = req.bearer_auth(key);
            }
            let resp = req.send().await?;
            if !resp.status().is_success() {
                anyhow::bail!("HTTP {}", resp.status());
            }
            resp.json::<serde_json::Value>().await.map_err(Into::into)
        })
    })?;
    Ok(parse_openai_model_ids(&json))
}

/// Extract model ids from an OpenAI `/v1/models` body (`data[].id`).
fn parse_openai_model_ids(json: &serde_json::Value) -> Vec<String> {
    json["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Extract model names from an Ollama `/api/tags` JSON body. Tolerant of a
/// missing / non-array `models` field (returns empty) and of entries without a
/// string `name`.
fn parse_model_names(json: &serde_json::Value) -> Vec<String> {
    json["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod model_list_tests {
    use super::parse_model_names;
    use serde_json::json;

    #[test]
    fn parses_names_and_tolerates_shape() {
        let names = parse_model_names(&json!({
            "models": [{"name": "llama3.1:8b"}, {"name": "gemma4:e2b"}, {"size": 1}]
        }));
        assert_eq!(
            names,
            vec!["llama3.1:8b".to_string(), "gemma4:e2b".to_string()]
        );
        // Missing or non-array `models` → empty, never a panic.
        assert!(parse_model_names(&json!({})).is_empty());
        assert!(parse_model_names(&json!({ "models": "nope" })).is_empty());
    }

    #[test]
    fn parses_openai_model_ids_and_tolerates_shape() {
        use super::parse_openai_model_ids;
        let ids = parse_openai_model_ids(&json!({
            "data": [{"id": "gpt-5", "object": "model"}, {"id": "claude"}, {"object": "x"}]
        }));
        assert_eq!(ids, vec!["gpt-5".to_string(), "claude".to_string()]);
        assert!(parse_openai_model_ids(&json!({})).is_empty());
        assert!(parse_openai_model_ids(&json!({ "data": 5 })).is_empty());
    }

    #[test]
    fn openai_usage_parses_or_none() {
        use super::openai_usage;
        let u = openai_usage(&json!({"prompt_tokens": 12, "completion_tokens": 34})).unwrap();
        assert_eq!(u.input_tokens, 12);
        assert_eq!(u.output_tokens, 34);
        // Missing either field → None (no partial/garbage usage).
        assert!(openai_usage(&json!({"prompt_tokens": 12})).is_none());
        assert!(openai_usage(&json!({})).is_none());
    }
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
        // Verified at compile time — use const assert to satisfy clippy.
        const _: () = {
            assert!(LOGO_10_COLS < LOGO_20_COLS);
            assert!(LOGO_20_COLS < LOGO_40_COLS);
            assert!(LOGO_40_COLS < LOGO_FULL_COLS);
            assert!(LOGO_FULL_COLS < LOGO_120_COLS);
            assert!(LOGO_120_COLS < LOGO_160_COLS);
        };
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
    fn config_helpers_read_from_passed_config_not_disk() {
        // Regression (#150): these helpers used to call Config::resolve() — a
        // disk read + TOML parse — on every invocation, several times per turn.
        // They now derive from the &Config threaded in, so run_chat resolves
        // once and reuses it. This test passes a value-bearing Config and proves
        // the returned values come from the argument, not from a config file on
        // disk (which the old, arg-less signatures could not have read).
        let cfg = newt_core::Config {
            tui: Some(newt_core::TuiConfig {
                max_tool_rounds: 7,
                tool_output_lines: 3,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(max_tool_rounds(&cfg), 7);
        assert_eq!(tool_output_lines(&cfg), 3);
        assert_eq!(resolve_tui(&cfg).map(|t| t.max_tool_rounds), Some(7));

        // An empty config yields the documented defaults.
        let empty = newt_core::Config::default();
        assert_eq!(max_tool_rounds(&empty), 25);
        assert_eq!(tool_output_lines(&empty), 20);
        assert_eq!(resolve_tui(&empty), None);
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

    // -----------------------------------------------------------------------
    // warmup helpers
    // -----------------------------------------------------------------------

    /// `is_model_resident` returns `false` for a model not in the `/api/ps` list.
    #[tokio::test(flavor = "multi_thread")]
    async fn is_model_resident_returns_false_when_absent() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"name": "other-model:7b"}]
            })))
            .mount(&server)
            .await;

        assert!(!is_model_resident(&server.uri(), "wanted-model:13b"));
    }

    /// `is_model_resident` returns `true` when the model appears in `/api/ps`.
    #[tokio::test(flavor = "multi_thread")]
    async fn is_model_resident_returns_true_when_present() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    {"name": "other:7b"},
                    {"name": "wanted-model:13b"}
                ]
            })))
            .mount(&server)
            .await;

        assert!(is_model_resident(&server.uri(), "wanted-model:13b"));
    }

    /// `is_model_resident` returns `false` (safe default) when the endpoint
    /// returns an error — the caller falls through to the warm-up path.
    #[tokio::test(flavor = "multi_thread")]
    async fn is_model_resident_returns_false_on_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        assert!(!is_model_resident(&server.uri(), "any-model"));
    }
}

/// Regression tests for the `run_command` → agent-bridle confined-shell unification.
///
/// The headline property: the WHOLE command is confined under the granted
/// `Caveats`, not just the leading token. On the old leading-token + `sh -c`
/// path, `echo ok && rm -rf /` passed the `echo` check and then ran `rm`
/// directly. Routing through agent-bridle's brush interceptor closes that
/// bypass — every external spawn passes the leash's `before_exec` gate.
#[cfg(test)]
mod run_command_confinement_tests {
    use super::*;
    use newt_core::caveats::{Caveats, CountBound, Scope};

    /// A `Caveats` granting exec for the given commands and full fs/read+write
    /// (so the test's own file-survival assertions are not themselves confined),
    /// otherwise read-only-ish. `exec` is `Scope::Only` of the named commands.
    #[cfg(unix)]
    fn caveats_exec_only(cmds: &[&str]) -> Caveats {
        Caveats {
            fs_read: Scope::All,
            fs_write: Scope::All,
            exec: Scope::Only(cmds.iter().map(|s| s.to_string()).collect()),
            net: Scope::none(),
            max_calls: CountBound::Unlimited,
            valid_for_generation: Scope::All,
        }
    }

    /// run_command with a caveats granting exec for a real external (`env`)
    /// succeeds: the command runs through the confined shell and returns its
    /// output (exit 0), no denial.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_command_allowed_external_succeeds() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = caveats_exec_only(&["env"]);
        let args = serde_json::json!({ "command": "env" });
        let out = execute_tool(
            "run_command",
            &args,
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut Mcp::empty(),
            None,
        )
        .await;
        assert!(
            !out.starts_with("capability denied"),
            "an in-scope external must not be denied, got: {out}"
        );
        assert!(
            !out.starts_with("error:"),
            "an in-scope external must run cleanly, got: {out}"
        );
    }

    /// run_command with an out-of-scope command is DENIED via the structured
    /// envelope field — surfaced as the capability-denied UX string, NOT a
    /// stderr grep.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_command_out_of_scope_is_denied() {
        let ws = tempfile::TempDir::new().unwrap();
        // Grant only `echo`; ask to run the external `env`.
        let caveats = caveats_exec_only(&["echo"]);
        let args = serde_json::json!({ "command": "env" });
        let out = execute_tool(
            "run_command",
            &args,
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut Mcp::empty(),
            None,
        )
        .await;
        assert!(
            out.starts_with("capability denied"),
            "an out-of-scope external must be denied via the structured field, got: {out}"
        );
        assert!(
            out.contains("[tui.permissions] extra_exec = [\"env\"]"),
            "exec denials should explain the extra_exec escape hatch, got: {out}"
        );
    }

    #[test]
    fn exec_denial_guidance_escapes_toml_literal() {
        let envelope = serde_json::json!({
            "denied": true,
            "denials": [
                {
                    "kind": "exec",
                    "target": "bad\"cmd",
                    "reason": "exec bad command denied"
                }
            ]
        });
        let reason = envelope_denial_reason_with_guidance(&envelope);
        assert!(reason.contains("[tui.permissions] extra_exec = [\"bad\\\"cmd\"]"));
    }

    #[test]
    fn exec_denial_guidance_uses_command_name_for_absolute_paths() {
        let envelope = serde_json::json!({
            "denied": true,
            "denials": [
                {
                    "kind": "exec",
                    "target": "/usr/bin/env",
                    "reason": "exec of \"/usr/bin/env\" is not within the granted authority"
                }
            ]
        });
        let reason = envelope_denial_reason_with_guidance(&envelope);
        assert!(reason.contains("[tui.permissions] extra_exec = [\"env\"]"));
        assert!(!reason.contains("extra_exec = [\"/usr/bin/env\"]"));
    }

    #[test]
    fn exec_denial_guidance_uses_command_name_for_windows_paths() {
        let envelope = serde_json::json!({
            "denied": true,
            "denials": [
                {
                    "kind": "exec",
                    "target": "C:\\tools\\env.exe",
                    "reason": "exec of \"C:\\tools\\env.exe\" is not within the granted authority"
                }
            ]
        });
        let reason = envelope_denial_reason_with_guidance(&envelope);
        assert!(reason.contains("[tui.permissions] extra_exec = [\"env.exe\"]"));
        assert!(!reason.contains("extra_exec = [\"C:\\\\tools\\\\env.exe\"]"));
    }

    /// THE test that justifies the change. `echo ok && rm -r <victim>` under a
    /// grant that allows `echo` but NOT `rm`: the `rm` is DENIED inside the
    /// confined shell and the victim file SURVIVES. On the old leading-token +
    /// `sh -c` path the `echo` check passed and `rm` then ran directly, deleting
    /// the victim. Full-command confinement is what stops it here.
    #[cfg(unix)]
    #[tokio::test]
    async fn compound_command_denies_ungranted_rm_and_victim_survives() {
        let ws = tempfile::TempDir::new().unwrap();
        let victim = ws.path().join("victim.txt");
        std::fs::write(&victim, b"do not delete me").unwrap();
        assert!(victim.exists(), "precondition: victim file exists");

        // Grant `echo` only — NOT `rm`.
        let caveats = caveats_exec_only(&["echo"]);
        let victim_str = victim.to_string_lossy();
        let args = serde_json::json!({
            "command": format!("echo ok && rm -r {victim_str}"),
        });
        let out = execute_tool(
            "run_command",
            &args,
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut Mcp::empty(),
            None,
        )
        .await;

        // The victim MUST survive: the `rm` never ran (leash denied the spawn).
        assert!(
            victim.exists(),
            "victim file must survive — the ungranted `rm` must be denied by the \
             confined shell (this would have slipped past the old leading-token \
             + `sh -c` path). run_command returned: {out}"
        );
    }

    /// read_file still enforces fs_read and returns contents (no regression
    /// from the run_command rewrite).
    #[tokio::test]
    async fn read_file_still_works() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("a.txt"), b"hello").unwrap();
        let caveats = Caveats {
            fs_read: Scope::All,
            fs_write: Scope::none(),
            exec: Scope::none(),
            net: Scope::none(),
            max_calls: CountBound::Unlimited,
            valid_for_generation: Scope::All,
        };
        let args = serde_json::json!({ "path": "a.txt" });
        let out = execute_tool(
            "read_file",
            &args,
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut Mcp::empty(),
            None,
        )
        .await;
        assert_eq!(out, "hello", "read_file must still return file contents");
    }

    /// write_file still enforces fs_write and writes the file (no regression).
    /// fs_write is scoped to the workspace (not `Scope::All`) so the y/N prompt
    /// is skipped — the preset is the consent.
    #[tokio::test]
    async fn write_file_still_works() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = Caveats {
            fs_read: Scope::All,
            fs_write: Scope::only([ws.path().to_string_lossy().into_owned()]),
            exec: Scope::none(),
            net: Scope::none(),
            max_calls: CountBound::Unlimited,
            valid_for_generation: Scope::All,
        };
        let args = serde_json::json!({ "path": "b.txt", "content": "written" });
        let out = execute_tool(
            "write_file",
            &args,
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut Mcp::empty(),
            None,
        )
        .await;
        assert!(
            out.starts_with("wrote"),
            "write_file must succeed, got: {out}"
        );
        assert_eq!(
            std::fs::read_to_string(ws.path().join("b.txt")).unwrap(),
            "written"
        );
    }

    /// list_dir still enforces fs_read and lists entries (no regression).
    #[tokio::test]
    async fn list_dir_still_works() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("one.txt"), b"x").unwrap();
        std::fs::write(ws.path().join("two.txt"), b"y").unwrap();
        let caveats = Caveats {
            fs_read: Scope::All,
            fs_write: Scope::none(),
            exec: Scope::none(),
            net: Scope::none(),
            max_calls: CountBound::Unlimited,
            valid_for_generation: Scope::All,
        };
        let args = serde_json::json!({ "path": "." });
        let out = execute_tool(
            "list_dir",
            &args,
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut Mcp::empty(),
            None,
        )
        .await;
        assert!(out.contains("one.txt") && out.contains("two.txt"));
    }
}

#[cfg(test)]
mod skills_integration_tests {
    use super::*;
    use std::fs;

    fn write_skill(root: &std::path::Path, name: &str, desc: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {desc}\n---\nFull body of {name}.\n"),
        )
        .unwrap();
    }

    #[test]
    fn system_prompt_index_includes_discovered_skill_name_and_description() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_skill(tmp.path(), "commit-style", "How this repo writes commits");

        let block = skills_index_for_prompt(&[tmp.path().to_path_buf()]).expect("an index block");
        assert!(block.contains("Available skills (call `use_skill` to load one):"));
        assert!(block.contains("commit-style: How this repo writes commits"));
        // Progressive disclosure: the body must NOT appear in the index.
        assert!(!block.contains("Full body of commit-style."));
    }

    #[test]
    fn system_prompt_index_is_none_when_no_skills() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(skills_index_for_prompt(&[tmp.path().to_path_buf()]).is_none());
    }

    #[test]
    fn system_prompt_index_unions_search_path_first_dir_wins() {
        // A skill of the same name in two dirs: the first dir on the path wins.
        let a = tempfile::TempDir::new().unwrap();
        let b = tempfile::TempDir::new().unwrap();
        write_skill(a.path(), "commit-style", "newt copy");
        write_skill(b.path(), "commit-style", "claude copy");
        write_skill(b.path(), "judge", "scoring");

        let block = skills_index_for_prompt(&[a.path().to_path_buf(), b.path().to_path_buf()])
            .expect("an index block");
        // First dir's description wins; second dir's same-named skill is shadowed.
        assert!(block.contains("commit-style: newt copy"));
        assert!(!block.contains("claude copy"));
        // But unique skills from later dirs are still included.
        assert!(block.contains("judge: scoring"));
    }

    #[test]
    fn system_prompt_fallback_uses_canonical_default_soul() {
        // Regression: the no-soul fallback used to be a private copy of the
        // identity string that drifted from newt-core's DEFAULT_SOUL. It must
        // now embed the canonical constant verbatim so the two can't diverge.
        let tmp = tempfile::TempDir::new().unwrap();
        let prompt = build_system_prompt_with_soul(tmp.path().to_str().unwrap(), None);
        assert!(
            prompt.contains(newt_core::DEFAULT_SOUL),
            "fallback must embed newt_core::DEFAULT_SOUL verbatim"
        );
    }

    #[test]
    fn system_prompt_includes_active_persona_overlay() {
        let tmp = tempfile::TempDir::new().unwrap();
        let persona = Persona {
            name: "reviewer".to_string(),
            prompt: "Review from a persona file.".to_string(),
            path: tmp.path().join("personas").join("reviewer.md"),
        };
        let prompt = build_system_prompt_with_persona(
            tmp.path().to_str().unwrap(),
            Some(newt_core::DEFAULT_SOUL),
            Some(&persona),
        );
        assert!(prompt.contains("Active persona: reviewer"));
        assert!(prompt.contains("Review from a persona file."));
    }

    #[test]
    fn persona_commands_parse_expected_actions() {
        assert_eq!(
            parse_persona_command("/persona reviewer").unwrap(),
            PersonaCommand::Set("reviewer".into())
        );
        assert_eq!(
            parse_persona_command("/persona set security").unwrap(),
            PersonaCommand::Set("security".into())
        );
        assert_eq!(
            parse_persona_command("/persona clear").unwrap(),
            PersonaCommand::Clear
        );
        assert_eq!(
            parse_persona_command("/persona show").unwrap(),
            PersonaCommand::Show
        );
        assert_eq!(
            parse_persona_command("/persona list").unwrap(),
            PersonaCommand::List
        );
        assert_eq!(
            parse_persona_command("/persona default").unwrap(),
            PersonaCommand::Set("coder".into())
        );
    }

    #[test]
    fn persona_store_writes_coder_default_only_when_loaded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("personas");
        let store = PersonaStore::new(dir.clone());

        assert!(
            !dir.exists(),
            "constructing a store must not write defaults"
        );

        let persona = store.load("coder").unwrap();

        assert_eq!(persona.name, "coder");
        assert_eq!(persona.path, dir.join("coder.md"));
        assert!(persona.prompt.contains(newt_core::DEFAULT_SOUL));
        assert!(
            dir.join("coder.md").is_file(),
            "first persona load should materialize the default coder file"
        );
    }

    #[test]
    fn persona_store_does_not_seed_non_empty_persona_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("personas");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("reviewer.md"), "Review from disk.").unwrap();
        let store = PersonaStore::new(dir.clone());

        let personas = store.list().unwrap();

        assert_eq!(personas.len(), 1);
        assert_eq!(personas[0].name, "reviewer");
        assert!(!dir.join("coder.md").exists());
    }

    #[tokio::test]
    async fn persona_set_starts_fresh_conversation_with_overlay() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_str().unwrap();
        let persona_dir = tmp.path().join("personas");
        fs::create_dir_all(&persona_dir).unwrap();
        fs::write(persona_dir.join("reviewer.md"), "Review from disk.").unwrap();
        let store = PersonaStore::new(persona_dir);
        let mut memory = newt_core::MemoryManager::new();
        memory.add_provider(newt_core::RollingWindow::new(5));
        memory
            .sync_all("old task", "old reply", &newt_core::TurnMetrics::default())
            .await;
        let mut system = rebuild_system_prompt(workspace, &memory, None);
        let mut active_persona = None;

        let message = handle_persona_command(
            "/persona reviewer",
            workspace,
            &store,
            &mut memory,
            &mut system,
            &mut active_persona,
        )
        .unwrap();

        assert_eq!(
            message,
            "Started a new conversation with persona `reviewer`."
        );
        assert_eq!(
            active_persona.as_ref().map(|p| p.name.as_str()),
            Some("reviewer")
        );
        assert!(system.contains("Active persona: reviewer"));
        assert!(system.contains("Review from disk."));
        let messages = memory.build_messages(&system, "new task");
        assert!(!messages.iter().any(|m| m.content == "old task"));
        assert!(!messages.iter().any(|m| m.content == "old reply"));
    }

    #[tokio::test]
    async fn new_conversation_preserves_active_persona() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_str().unwrap();
        let mut memory = newt_core::MemoryManager::new();
        memory.add_provider(newt_core::RollingWindow::new(5));
        memory
            .sync_all("old task", "old reply", &newt_core::TurnMetrics::default())
            .await;
        let active_persona = Some(Persona {
            name: "terse".to_string(),
            prompt: "Keep replies short.".to_string(),
            path: tmp.path().join("personas").join("terse.md"),
        });
        let mut system = rebuild_system_prompt(workspace, &memory, active_persona.as_ref());

        let message =
            handle_new_conversation(workspace, &mut memory, &mut system, active_persona.as_ref());

        assert_eq!(message, "Started a new conversation with persona `terse`.");
        assert!(system.contains("Active persona: terse"));
        assert!(system.contains("Keep replies short."));
        let messages = memory.build_messages(&system, "new task");
        assert!(!messages.iter().any(|m| m.content == "old task"));
        assert!(!messages.iter().any(|m| m.content == "old reply"));
    }

    #[test]
    fn use_skill_tool_is_advertised_in_definitions() {
        let defs = tool_definitions();
        let names: Vec<&str> = defs
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .collect();
        assert!(names.contains(&"use_skill"), "got: {names:?}");
    }
}

// ---------------------------------------------------------------------------
// Tool-call round cap + graceful cap-exit (issue: configurable max_tool_rounds)
// ---------------------------------------------------------------------------
//
// These tests exercise both agentic loops (`chat_complete` -> Ollama path and
// `openai_chat_complete`) against a wiremock backend. The mock returns tool
// calls while `tools` are present in the request and a real text answer once
// they are absent — letting us assert that:
//   (1) the loop honours the configured `max_tool_rounds` cap, and
//   (2) on hitting the cap newt issues ONE final tools-disabled completion and
//       returns its text (NOT the `(reached tool-call limit)` placeholder).
#[cfg(test)]
mod tool_round_cap_tests {
    use super::*;
    use newt_core::caveats::Caveats;
    use newt_core::{BackendKind, MemMessage};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    /// Was the `"tools"` key present on this request body?
    fn request_has_tools(req: &Request) -> bool {
        serde_json::from_slice::<serde_json::Value>(&req.body)
            .ok()
            .map(|v| v.get("tools").is_some())
            .unwrap_or(false)
    }

    /// Ollama-shaped responder: returns a tool call whenever `tools` are
    /// offered, and a plain text answer once they are withheld. Counts the
    /// number of tool-offering requests it served.
    struct OllamaResponder {
        tool_rounds_served: Arc<AtomicUsize>,
        final_answer: String,
    }

    impl Respond for OllamaResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if request_has_tools(req) {
                self.tool_rounds_served.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "tool_calls": [{
                            "function": { "name": "definitely_not_a_real_tool", "arguments": {} }
                        }]
                    }
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": self.final_answer }
                }))
            }
        }
    }

    /// OpenAI-shaped responder: same logic, OpenAI `choices[0].message` shape.
    struct OpenAiResponder {
        tool_rounds_served: Arc<AtomicUsize>,
        final_answer: String,
    }

    impl Respond for OpenAiResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if request_has_tools(req) {
                self.tool_rounds_served.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "definitely_not_a_real_tool", "arguments": "{}" }
                        }]
                    }}]
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": self.final_answer } }]
                }))
            }
        }
    }

    fn msgs() -> Vec<MemMessage> {
        vec![
            MemMessage::system("you are a test"),
            MemMessage::user("do the thing"),
        ]
    }

    #[tokio::test]
    async fn ollama_loop_honors_configured_cap_and_returns_real_final_answer() {
        let server = MockServer::start().await;
        let served = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(OllamaResponder {
                tool_rounds_served: served.clone(),
                final_answer: "here is my partial summary".into(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let cap = 3;
        let (reply, streamed, _usage, _hallu) = chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                caveats: &caveats,
                max_tool_rounds: cap,
                tool_output_lines: 20,
                debug: false,
                num_ctx: None,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                build_check_cmd: None,
            },
            &mut Mcp::empty(),
        )
        .await
        .expect("chat_complete should succeed");

        // The cap was honoured: exactly `cap` tool-offering rounds were served.
        assert_eq!(served.load(Ordering::SeqCst), cap);
        // The cap-exit issued a final tools-disabled completion and returned
        // its text — NOT the dead placeholder.
        assert_eq!(reply, "here is my partial summary");
        assert_ne!(reply, "(reached tool-call limit)");
        assert!(!streamed);
    }

    #[tokio::test]
    async fn openai_loop_honors_configured_cap_and_returns_real_final_answer() {
        let server = MockServer::start().await;
        let served = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(OpenAiResponder {
                tool_rounds_served: served.clone(),
                final_answer: "openai partial answer".into(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let cap = 2;
        let (reply, streamed, _usage, _hallu) = openai_chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Openai,
                api_key: Some("sk-test"),
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                caveats: &caveats,
                max_tool_rounds: cap,
                tool_output_lines: 20,
                debug: false,
                num_ctx: None,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                build_check_cmd: None,
            },
            &mut Mcp::empty(),
        )
        .await
        .expect("openai_chat_complete should succeed");

        assert_eq!(served.load(Ordering::SeqCst), cap);
        assert_eq!(reply, "openai partial answer");
        assert_ne!(reply, "(reached tool-call limit)");
        assert!(!streamed);
    }

    #[tokio::test]
    async fn cap_exit_fallback_when_final_summary_errors() {
        // No mock for the tools-disabled request would still 404 via the
        // tool-offering mock only matching when... actually both match the same
        // path, so instead we mount a server that always 500s the *second*
        // shape. Simpler: a server that returns tool calls for tools-present
        // and a 500 for tools-absent, forcing the fallback branch.
        let server = MockServer::start().await;
        let served = Arc::new(AtomicUsize::new(0));
        struct ErrOnFinal {
            served: Arc<AtomicUsize>,
        }
        impl Respond for ErrOnFinal {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                if request_has_tools(req) {
                    self.served.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": { "content": "", "tool_calls": [{
                            "function": { "name": "definitely_not_a_real_tool", "arguments": {} }
                        }]}
                    }))
                } else {
                    ResponseTemplate::new(500).set_body_string("boom")
                }
            }
        }
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ErrOnFinal {
                served: served.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let (reply, _streamed, _usage, _hallu) = chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                caveats: &caveats,
                max_tool_rounds: 2,
                tool_output_lines: 20,
                debug: false,
                num_ctx: None,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                build_check_cmd: None,
            },
            &mut Mcp::empty(),
        )
        .await
        .expect("chat_complete should succeed even when final summary errors");

        // Fallback names the limit + the knob — strictly better than the bare
        // placeholder.
        assert!(reply.contains("tool-call limit"));
        assert!(reply.contains("max_tool_rounds"));
    }

    // -----------------------------------------------------------------------
    // Hallucination tracker + accumulated usage tests
    // -----------------------------------------------------------------------

    /// `run_command` called with a tool name as the first word must return a
    /// corrective error message, not shell it through agent-bridle.
    #[tokio::test]
    async fn run_command_refuses_tool_name_as_shell_command() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = Caveats::top();
        for tool in [
            "list_dir",
            "read_file",
            "write_file",
            "use_skill",
            "web_fetch",
        ] {
            let args = serde_json::json!({ "command": format!("{tool} some/path") });
            let out = execute_tool(
                "run_command",
                &args,
                &ws.path().to_string_lossy(),
                false,
                20,
                &caveats,
                &mut Mcp::empty(),
                None,
            )
            .await;
            assert!(
                out.contains("is a tool, not a shell command"),
                "expected corrective message for '{tool}', got: {out}"
            );
        }
    }

    /// `is_hallucination` correctly identifies tool-name-as-command and unknown
    /// tool names, and correctly skips MCP-namespaced tools.
    #[test]
    fn hallucination_detection_coverage() {
        // tool name passed to run_command → hallucination
        assert!(is_hallucination(
            "run_command",
            &serde_json::json!({"command": "list_dir ."})
        ));
        // normal shell command → not a hallucination
        assert!(!is_hallucination(
            "run_command",
            &serde_json::json!({"command": "cargo test"})
        ));
        // unknown tool → hallucination
        assert!(is_hallucination(
            "definitely_not_a_real_tool",
            &serde_json::json!({})
        ));
        // MCP-namespaced tool → not a hallucination
        assert!(!is_hallucination(
            "my_server__some_tool",
            &serde_json::json!({})
        ));
        // known direct tools → not hallucinations when called correctly
        for t in [
            "list_dir",
            "read_file",
            "write_file",
            "use_skill",
            "web_fetch",
        ] {
            assert!(!is_hallucination(t, &serde_json::json!({"path": "."})));
        }
    }

    /// `trim_for_summary` keeps head + tail and inserts a placeholder for
    /// the dropped middle section.
    #[test]
    fn trim_for_summary_drops_middle_and_inserts_placeholder() {
        let msgs: Vec<serde_json::Value> = (0..10)
            .map(|i| serde_json::json!({"role": "user", "content": format!("msg {i}")}))
            .collect();

        let trimmed = trim_for_summary(&msgs, 2, 3);
        // head(2) + placeholder(1) + tail(3) = 6
        assert_eq!(
            trimmed.len(),
            6,
            "expected 6 messages, got {}",
            trimmed.len()
        );
        // First two are the original head
        assert_eq!(trimmed[0]["content"], "msg 0");
        assert_eq!(trimmed[1]["content"], "msg 1");
        // Placeholder in the middle
        let placeholder = trimmed[2]["content"].as_str().unwrap();
        assert!(
            placeholder.contains("omitted"),
            "placeholder must mention omitted messages: {placeholder}"
        );
        // Last three are the original tail
        assert_eq!(trimmed[3]["content"], "msg 7");
        assert_eq!(trimmed[4]["content"], "msg 8");
        assert_eq!(trimmed[5]["content"], "msg 9");
    }

    #[test]
    fn trim_for_summary_passthrough_when_short_enough() {
        let msgs: Vec<serde_json::Value> = (0..4)
            .map(|i| serde_json::json!({"role": "user", "content": format!("msg {i}")}))
            .collect();
        // head=2, tail=3 → total=5, msgs.len()=4 → no trimming needed
        let trimmed = trim_for_summary(&msgs, 2, 3);
        assert_eq!(trimmed.len(), 4);
    }

    // -----------------------------------------------------------------------
    // repair_orphaned_tool_calls tests
    // -----------------------------------------------------------------------

    /// A complete tool_calls + tool_result pair is left untouched.
    #[test]
    fn repair_leaves_matched_tool_calls_intact() {
        let mut msgs = vec![
            serde_json::json!({"role": "user", "content": "do it"}),
            serde_json::json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"function": {"name": "list_dir", "arguments": {}}}]
            }),
            serde_json::json!({"role": "tool", "content": "file.rs"}),
        ];
        repair_orphaned_tool_calls(&mut msgs);
        // The assistant message must still have tool_calls.
        assert!(
            msgs[1]["tool_calls"].as_array().is_some(),
            "matched tool_calls must be preserved"
        );
    }

    /// An assistant message whose tool_calls have no following tool result
    /// gets tool_calls stripped — Anthropic/Bedrock would 400 otherwise.
    #[test]
    fn repair_strips_orphaned_tool_calls() {
        let mut msgs = vec![
            serde_json::json!({"role": "user", "content": "first"}),
            serde_json::json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"function": {"name": "list_dir", "arguments": {}}}]
            }),
            // Placeholder from trim — NOT a tool result.
            serde_json::json!({"role": "user", "content": "[context omitted]"}),
            serde_json::json!({"role": "assistant", "content": "done"}),
        ];
        repair_orphaned_tool_calls(&mut msgs);
        assert!(
            msgs[1].get("tool_calls").is_none(),
            "orphaned tool_calls must be stripped"
        );
        // Content should be preserved or a placeholder injected.
        assert!(
            msgs[1]["content"].as_str().is_some(),
            "assistant message must still have content after stripping tool_calls"
        );
    }

    /// trim_for_summary followed by repair produces no orphaned tool_calls,
    /// matching the Bedrock/Anthropic requirement.
    #[test]
    fn trim_then_repair_produces_no_orphans() {
        // Build a conversation: user → (assistant+tool_calls → tool_result) × 5
        let mut msgs = vec![serde_json::json!({"role": "user", "content": "task"})];
        for i in 0..5u32 {
            msgs.push(serde_json::json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"id": format!("call_{i}"), "function": {"name": "list_dir", "arguments": {}}}]
            }));
            msgs.push(serde_json::json!({"role": "tool", "tool_call_id": format!("call_{i}"), "content": "result"}));
        }
        // Trim aggressively (head=1, tail=2) — cuts through tool pairs.
        let trimmed = trim_for_summary(&msgs, 1, 2);
        // After trim+repair, every remaining tool_calls must have ALL its IDs
        // covered by a role="tool" result present somewhere in the list.
        let result_ids: std::collections::HashSet<String> = trimmed
            .iter()
            .filter(|m| m["role"].as_str() == Some("tool"))
            .filter_map(|m| m["tool_call_id"].as_str().map(|s| s.to_string()))
            .collect();
        for msg in &trimmed {
            if msg["role"].as_str() == Some("assistant") {
                if let Some(tc) = msg["tool_calls"].as_array() {
                    for call in tc {
                        let id = call["id"].as_str().unwrap_or("");
                        assert!(
                            result_ids.contains(id),
                            "after trim+repair, tool_call id={id:?} has no matching tool result"
                        );
                    }
                }
            }
        }
    }

    /// Regression: assistant with TWO tool_calls where only the first result
    /// survives trimming must have ALL tool_calls stripped (not just partially).
    /// The old code checked only "next message is role=tool" — this was enough
    /// for single-call rounds but missed the second ID in a multi-call round,
    /// causing Bedrock to return 400 "Expected toolResult blocks".
    #[test]
    fn repair_strips_partial_tool_call_results() {
        // Simulate trim output: assistant called tc_a + tc_b but only tc_a's
        // result survived — tc_b was dropped in the middle.
        let mut msgs = vec![
            serde_json::json!({"role": "user", "content": "task"}),
            serde_json::json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {"id": "tc_a", "function": {"name": "read_file", "arguments": {}}},
                    {"id": "tc_b", "function": {"name": "list_dir",  "arguments": {}}}
                ]
            }),
            // Only tc_a's result is present; tc_b's was trimmed.
            serde_json::json!({"role": "tool", "tool_call_id": "tc_a", "content": "file content"}),
            serde_json::json!({"role": "assistant", "content": "done"}),
        ];
        repair_orphaned_tool_calls(&mut msgs);
        // The incomplete assistant must have tool_calls stripped.
        assert!(
            msgs[1].get("tool_calls").is_none(),
            "partial tool_calls (tc_b missing) must be stripped"
        );
        // The now-orphaned tc_a result must also be removed.
        let has_orphaned_result = msgs.iter().any(|m| {
            m["role"].as_str() == Some("tool") && m["tool_call_id"].as_str() == Some("tc_a")
        });
        assert!(
            !has_orphaned_result,
            "tool_result for stripped tool_call must be removed"
        );
    }

    /// Regression: orphaned role="tool" at the start of the tail (its assistant
    /// was dropped by trimming) must be removed.
    #[test]
    fn repair_removes_orphaned_tool_result() {
        let mut msgs = vec![
            serde_json::json!({"role": "user",      "content": "task"}),
            serde_json::json!({"role": "user",      "content": "[N messages omitted]"}),
            // tc_old's assistant was dropped — this result is now orphaned.
            serde_json::json!({"role": "tool", "tool_call_id": "tc_old", "content": "stale"}),
            serde_json::json!({"role": "assistant", "content": "done"}),
        ];
        repair_orphaned_tool_calls(&mut msgs);
        let has_orphan = msgs.iter().any(|m| {
            m["role"].as_str() == Some("tool") && m["tool_call_id"].as_str() == Some("tc_old")
        });
        assert!(
            !has_orphan,
            "orphaned tool_result with no matching assistant must be removed"
        );
    }

    /// When the final summary 500s, the accumulated usage from the tool rounds
    /// must still be returned (not None), so usage.jsonl is not blank.
    #[tokio::test]
    async fn accumulated_usage_survives_summary_failure() {
        let server = MockServer::start().await;
        let served = Arc::new(AtomicUsize::new(0));

        struct UsageRoundsErrFinal {
            served: Arc<AtomicUsize>,
        }
        impl Respond for UsageRoundsErrFinal {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                if request_has_tools(req) {
                    self.served.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": { "content": "", "tool_calls": [{
                            "function": { "name": "definitely_not_a_real_tool", "arguments": {} }
                        }]},
                        // Ollama reports per-round usage even in non-streaming mode.
                        "prompt_eval_count": 100,
                        "eval_count": 20,
                    }))
                } else {
                    ResponseTemplate::new(500).set_body_string("boom")
                }
            }
        }

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(UsageRoundsErrFinal {
                served: served.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let cap = 2;
        let (reply, _streamed, usage, hallu) = chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                caveats: &caveats,
                max_tool_rounds: cap,
                tool_output_lines: 20,
                debug: false,
                num_ctx: None,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                build_check_cmd: None,
            },
            &mut Mcp::empty(),
        )
        .await
        .expect("chat_complete must succeed even when final summary errors");

        // The fallback reply must contain accumulated token counts.
        assert!(reply.contains("tool-call limit"), "got: {reply}");
        assert!(
            reply.contains("in / ") && reply.contains("out tokens"),
            "fallback must include accumulated token counts, got: {reply}"
        );

        // The usage returned must be non-None and reflect the accumulated rounds.
        let u = usage.expect("usage must be Some even when final summary fails");
        assert_eq!(
            u.input_tokens, 200,
            "2 rounds × 100 input tokens each = 200 total"
        );
        assert_eq!(
            u.output_tokens, 40,
            "2 rounds × 20 output tokens each = 40 total"
        );

        // Unknown tool calls during cap rounds counted as hallucinations.
        assert_eq!(
            hallu, cap as u32,
            "each round had one hallucinated tool call"
        );
    }
}

// ---------------------------------------------------------------------------
// fd_exhaustion_tests — verify O_CLOEXEC marking and EMFILE detection
// ---------------------------------------------------------------------------

#[cfg(all(test, unix))]
mod fd_exhaustion_tests {
    use super::*;

    /// mark_fds_cloexec must never touch stdin/stdout/stderr.
    #[test]
    fn mark_fds_cloexec_preserves_stdio() {
        mark_fds_cloexec();
        // fds 0-2 must remain open and CLOEXEC-free.
        for fd in 0..3i32 {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(
                flags >= 0,
                "stdio fd {fd} must remain open after mark_fds_cloexec"
            );
            assert_eq!(
                flags & libc::FD_CLOEXEC,
                0,
                "stdio fd {fd} must not have FD_CLOEXEC set"
            );
        }
    }

    /// A freshly-opened fd that lacks CLOEXEC gets the flag set by mark_fds_cloexec.
    #[test]
    fn mark_fds_cloexec_sets_flag_on_new_fd() {
        let f = std::fs::File::open("/dev/null").expect("open /dev/null");
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&f);

        // Ensure CLOEXEC is NOT set initially (it may or may not be,
        // depending on the std implementation; clear it to be sure).
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0,
            "pre-condition: CLOEXEC should be clear"
        );

        mark_fds_cloexec();

        let flags_after = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_ne!(
            flags_after & libc::FD_CLOEXEC,
            0,
            "mark_fds_cloexec must set FD_CLOEXEC on an open fd (fd={fd})"
        );
    }

    /// Under normal conditions there is always at least one free fd slot.
    #[test]
    fn terminal_fd_available_true_normally() {
        assert!(
            terminal_fd_available(),
            "fd table should have free slots normally"
        );
    }

    /// After exhausting the fd table, terminal_fd_available returns false.
    /// After releasing, it returns true again.
    #[test]
    fn terminal_fd_available_false_when_exhausted_then_true_after_release() {
        // Open files until we hit EMFILE or a reasonable limit.
        let mut holders: Vec<std::fs::File> = Vec::new();
        let mut hit_limit = false;
        for _ in 0..4096 {
            match std::fs::File::open("/dev/null") {
                Ok(f) => holders.push(f),
                Err(e) if e.raw_os_error() == Some(libc::EMFILE) => {
                    hit_limit = true;
                    break;
                }
                Err(_) => break,
            }
        }

        if hit_limit {
            // At this point the fd table is full.
            assert!(
                !terminal_fd_available(),
                "terminal_fd_available must return false when fd table is full"
            );
        }

        // Release all holders — regardless of whether we hit the limit.
        drop(holders);

        // After release, the fd table has free slots again.
        assert!(
            terminal_fd_available(),
            "terminal_fd_available must return true after releasing fds"
        );
    }

    /// mark_fds_cloexec is idempotent — calling it twice changes nothing.
    #[test]
    fn mark_fds_cloexec_is_idempotent() {
        let f = std::fs::File::open("/dev/null").expect("open /dev/null");
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&f);
        mark_fds_cloexec();
        let flags_first = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        mark_fds_cloexec();
        let flags_second = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_eq!(
            flags_first, flags_second,
            "second call must not change fd flags"
        );
    }
}
