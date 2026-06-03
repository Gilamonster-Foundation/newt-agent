//! Newt-Agent TUI — a lean chat + agentic-coding TUI in the spirit of Codex /
//! Claude Code, deliberately scoped to *chat and agentic coding* (not as
//! feature-rich). Splash + chat REPL + slash commands + ocap-gated tool use.
//! NOT a settings UI: configuration is plain `~/.newt/config.toml`
//! (see `newt config`). Additional features and the multi-agent matrix live in
//! the downstream `gilamonster-agent`, which inherits these crates.

mod wizard;

/// Run the (non-interactive) setup wizard unconditionally — used by `newt init`.
/// Probes Ollama and (re)writes `~/.newt/config.toml`; edit that file for
/// anything else.
pub fn run_init(color: bool) -> anyhow::Result<()> {
    wizard::run_init(color)
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

pub fn run_code(path: Option<&std::path::Path>, no_splash: bool) -> anyhow::Result<()> {
    let color = color_supported_with(&|k| std::env::var(k).ok());

    // First-run wizard: silent no-op if config already exists.
    wizard::maybe_run(color)?;

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

/// Read-only enforcement caveats (read anything; no write/exec/net) — the safe
/// default when nothing is configured or identity setup fails.
fn read_only_caveats(workspace: &str) -> newt_core::caveats::Caveats {
    newt_core::ToolPermissions {
        preset: newt_core::PermissionPreset::ReadOnly,
        extra_exec: Vec::new(),
    }
    .to_caveats(workspace)
}

/// Lower the configured TUI permission policy to a `Caveats` value. With no
/// `[tui]` config the policy is **read-only** — never `Caveats::top()`. Pure in
/// its inputs, so the safe-default behavior is unit-testable.
fn policy_for(tui: Option<newt_core::TuiConfig>, workspace: &str) -> newt_core::caveats::Caveats {
    tui.map(|t| t.permissions.to_caveats(workspace))
        .unwrap_or_else(|| read_only_caveats(workspace))
}

/// Resolve the configured `[tui]` block, if any.
fn resolve_tui() -> Option<newt_core::TuiConfig> {
    newt_core::Config::resolve().ok().and_then(|c| c.tui)
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
    let history_path = newt_core::Config::user_config_path().map(|p| p.with_file_name("history"));

    // Use the existing tokio runtime from main — block_in_place lets rustyline
    // block the thread while still allowing block_on() inside it.
    let rt = tokio::runtime::Handle::current();

    // Resolve the inference backend and permission caveats once at session
    // start.  Both are re-read after each slash command (config.toml on disk).
    let mut choice = resolve_backend_choice();
    let (mut inf_url, mut inf_model) = (choice.url.clone(), choice.model.clone());
    let mut inf_kind = choice.kind;
    let mut inf_key = choice.api_key.clone();
    let key_path = newt_identity::default_key_path().ok();
    let mut cap = SessionCapability::establish(resolve_tui(), key_path.as_deref(), workspace);
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

    // system prompt is built AFTER initialize_all (see below) so soul files are loaded.
    // Placeholder until then.
    let system: String;

    // Pluggable memory manager — replaces the old conv Vec.
    let mem_cfg = newt_core::Config::resolve()
        .ok()
        .and_then(|c| c.memory)
        .unwrap_or_default();
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
    {
        let soul_additions = memory.build_system_prompt_additions();
        let soul_text = if soul_additions.is_empty() {
            None
        } else {
            Some(soul_additions.as_str())
        };
        system = build_system_prompt_with_soul(workspace, soul_text);
    }

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
                    let cont = dispatch_slash(&task, workspace, color, verbose)?;
                    if let Some(ref hp) = history_path {
                        let _ = rl.save_history(hp);
                    }
                    // Re-read config after a slash command (config.toml may have changed).
                    // Permissions can only NARROW within a session; a widening
                    // request is clamped (restart to widen — see SessionCapability).
                    choice = resolve_backend_choice();
                    inf_url = choice.url.clone();
                    inf_model = choice.model.clone();
                    inf_kind = choice.kind;
                    inf_key = choice.api_key.clone();
                    if cap.reapply(resolve_tui(), workspace) {
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
                    if !cont {
                        break;
                    }
                } else if matches!(task.as_str(), "exit" | "quit") {
                    break;
                } else {
                    print_thinking(color);
                    let t0 = std::time::Instant::now();

                    // Build message list from memory manager.
                    let messages = memory.build_messages(&system, &task);
                    let response = tokio::task::block_in_place(|| {
                        rt.block_on(chat_complete(ChatCtx {
                            url: &inf_url,
                            model: &inf_model,
                            kind: inf_kind,
                            api_key: inf_key.as_deref(),
                            messages: &messages,
                            task: &task,
                            workspace,
                            color,
                            caveats: cap.caveats(),
                            max_tool_rounds: max_tool_rounds(),
                        }))
                    });

                    let elapsed = t0.elapsed();
                    erase_line();
                    match response {
                        Ok((reply, was_streamed, usage)) => {
                            if !was_streamed {
                                print_newt(&reply, color, verbose);
                            }
                            // Single TurnMetrics used for both memory sync and display.
                            let pricing = newt_core::Config::resolve()
                                .ok()
                                .and_then(|c| c.pricing)
                                .unwrap_or_default();
                            let metrics = newt_core::TurnMetrics {
                                elapsed_ms: elapsed.as_millis() as u64,
                                usage,
                                cost_usd: pricing.estimate_cost(&inf_model, usage.as_ref()),
                                model_id: inf_model.clone(),
                                endpoint: inf_url.clone(),
                            };
                            tokio::task::block_in_place(|| {
                                rt.block_on(memory.sync_all(&task, &reply, &metrics));
                            });
                            print_metrics(&metrics, color);
                            // Append to usage log (best-effort).
                            if let Some(log) = newt_core::Config::user_config_path()
                                .map(|p| p.with_file_name("usage.jsonl"))
                            {
                                metrics.append_to_log(&log);
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
            newt_core::Config::resolve()
                .ok()
                .and_then(|c| c.dgx)
                .and_then(|d| d.nodes.into_iter().next())
                .and_then(|n| n.ollama)
        })
        .unwrap_or_else(|| "http://localhost:11434".into());

    let model = std::env::var("NEWT_DGX_MODEL")
        .ok()
        .or_else(|| {
            newt_core::Config::resolve()
                .ok()
                .and_then(|c| c.dgx)
                .and_then(|d| d.active_model)
        })
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
fn resolve_backend_choice() -> BackendChoice {
    if let Ok(cfg) = newt_core::Config::resolve() {
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
    }
    let (url, model) = resolve_backend_config();
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

fn build_system_prompt_with_soul(workspace: &str, soul: Option<&str>) -> String {
    let identity = soul.unwrap_or(
        "You are newt, a small, fast, local-first agentic coder. \
         Be concise and direct. \
         You have tools: run_command, read_file, write_file, list_dir. \
         Use them to actually complete tasks rather than describing what to do.",
    );
    let mut ctx = format!("{identity}\n\nWorkspace: {workspace}\n");

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
                "description": "Write or overwrite a file in the workspace (asks user to confirm before writing)",
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
        }
    ])
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
fn tool_output_lines() -> usize {
    newt_core::Config::resolve()
        .ok()
        .and_then(|c| c.tui)
        .map(|t| t.tool_output_lines)
        .unwrap_or(20)
}

/// Maximum tool-call rounds per turn, from `[tui].max_tool_rounds`.
/// Defaults to 25 when there's no `[tui]` table or no config file.
fn max_tool_rounds() -> usize {
    newt_core::Config::resolve()
        .ok()
        .and_then(|c| c.tui)
        .map(|t| t.max_tool_rounds)
        .unwrap_or(25)
}

/// Print tool output truncated to the configured line limit.
/// The model always receives the full content regardless.
fn print_tool_output(output: &str, color: bool) {
    if output.is_empty() {
        return;
    }
    let max = tool_output_lines();
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

/// Execute a single tool call and return the result string sent back to the model.
///
/// `run_command` is routed through agent-bridle's Caveats-confined, brush-backed
/// `shell` tool: the WHOLE command runs inside the leash (`echo ok && rm -rf /`
/// no longer slips `rm` past an `echo` grant — every external spawn passes the
/// interceptor's `before_exec` / `before_open` gate). The fs tools
/// (`read_file` / `write_file` / `list_dir`) keep enforcing the same `caveats`
/// via `permits_*` — rerouting them is out of scope.
async fn execute_tool(
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    caveats: &newt_core::caveats::Caveats,
) -> String {
    match name {
        "run_command" => {
            let cmd = args["command"].as_str().unwrap_or("");
            print_tool_call("run_command", cmd, color);

            // Route the WHOLE command through agent-bridle's confined shell
            // (free-form `cmd` mode) under the SAME Caveats the TUI resolved
            // from `[tui].permissions`. `caveats` is `newt_core::caveats::Caveats`,
            // a re-export of `agent_mesh_protocol::caveats::Caveats` — the exact
            // type `Registry::dispatch` expects, so no conversion is needed.
            let dispatch_args = serde_json::json!({
                "cmd": cmd,
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
                    let reason = envelope_denial_reason(&envelope);
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
                    print_tool_output(&out, color);
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
                    print_tool_output(&contents, color);
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
                        println!("✓ wrote {path}");
                        format!("wrote {path}")
                    }
                    Err(e) => format!("error writing {path}: {e}"),
                }
            } else {
                println!("skipped");
                format!("user declined to write {path}")
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
                    print_tool_output(&listing, color);
                    listing
                }
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
}

/// Main agentic loop: call model → execute tool calls → feed results back → repeat.
/// Returns `(reply_text, was_streamed, token_usage)`.
/// When `was_streamed` is true the text was already printed token-by-token.
async fn chat_complete(
    ctx: ChatCtx<'_>,
) -> anyhow::Result<(String, bool, Option<newt_core::TokenUsage>)> {
    // OpenAI-compatible endpoints speak a different wire format (request,
    // tool_calls, and usage shapes all differ), so they get their own loop.
    if ctx.kind == newt_core::BackendKind::Openai {
        return openai_chat_complete(ctx).await;
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
    } = ctx;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let chat_url = format!("{}/api/chat", url.trim_end_matches('/'));

    // Convert MemMessage list to Ollama JSON format.
    // The memory manager already included the current task as the last user message.
    let mut messages: Vec<serde_json::Value> = mem_messages
        .iter()
        .map(|m| serde_json::json!({"role": m.role.as_str(), "content": m.content}))
        .collect();

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

        // Tool-call rounds: stream:false (fast, just JSON).
        // Final text round: stream:true so the user sees tokens arrive.
        // We don't know which round is last, so we probe with stream:false first
        // and switch to streaming only when the model returns no tool calls.
        let body_no_stream = serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": false,
            "tools": tool_definitions(),
        });

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

        let json: serde_json::Value = resp.json().await?;
        let message = &json["message"];

        let tool_calls = message["tool_calls"].as_array();
        let has_tools = tool_calls.map(|tc| !tc.is_empty()).unwrap_or(false);

        if !has_tools {
            // No tool calls — re-issue with stream:true so the user sees tokens.
            // `messages` already contains the task; just replay with streaming.
            let body_stream = serde_json::json!({
                "model": model,
                "messages": &messages,
                "stream": true,
                "tools": tool_definitions(),
            });
            let sresp = client
                .post(&chat_url)
                .json(&body_stream)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("stream request failed: {e}"))?;

            if !sresp.status().is_success() {
                let content = message["content"].as_str().unwrap_or("").to_string();
                return Ok((content, false, None));
            }
            let (streamed, usage) = stream_response(sresp, color).await?;
            return Ok((streamed, true, usage));
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
            let result = execute_tool(name, &args, workspace, color, caveats).await;
            messages.push(serde_json::json!({
                "role": "tool",
                "content": result
            }));
        }
    }

    // Reached the round cap without a tool-free answer. Make ONE final
    // completion with tools DISABLED so the model summarises what it found
    // and the user gets a real (partial) answer instead of a placeholder.
    final_summary_ollama(&client, &chat_url, model, &mut messages, max_tool_rounds).await
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
/// fails. Names the limit and points at the knob to raise it — strictly more
/// useful than the bare `(reached tool-call limit)` placeholder.
fn cap_exit_fallback(max_tool_rounds: usize) -> String {
    format!(
        "(reached the tool-call limit of {max_tool_rounds} rounds, and the \
         final summarization request also failed — raise [tui].max_tool_rounds \
         in your newt config to allow more rounds)"
    )
}

/// Final tools-disabled completion for the Ollama (`/api/chat`) path.
async fn final_summary_ollama(
    client: &reqwest::Client,
    chat_url: &str,
    model: &str,
    messages: &mut Vec<serde_json::Value>,
    max_tool_rounds: usize,
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
    match client.post(chat_url).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => {
            let json: serde_json::Value = resp.json().await?;
            let content = json["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            if content.is_empty() {
                Ok((cap_exit_fallback(max_tool_rounds), false, None))
            } else {
                Ok((content, false, None))
            }
        }
        _ => Ok((cap_exit_fallback(max_tool_rounds), false, None)),
    }
}

/// Final tools-disabled completion for the OpenAI (`/v1/chat/completions`) path.
async fn final_summary_openai(
    client: &reqwest::Client,
    chat_url: &str,
    model: &str,
    api_key: Option<&str>,
    messages: &mut Vec<serde_json::Value>,
    max_tool_rounds: usize,
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
    let mut req = client.post(chat_url).json(&body);
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            let json: serde_json::Value = resp.json().await?;
            let content = json["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let usage = openai_usage(&json["usage"]);
            if content.is_empty() {
                Ok((cap_exit_fallback(max_tool_rounds), false, usage))
            } else {
                Ok((content, false, usage))
            }
        }
        _ => Ok((cap_exit_fallback(max_tool_rounds), false, None)),
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
) -> anyhow::Result<(String, bool, Option<newt_core::TokenUsage>)> {
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
    } = ctx;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let chat_url = format!("{}/v1/chat/completions", url.trim_end_matches('/'));

    let mut messages: Vec<serde_json::Value> = mem_messages
        .iter()
        .map(|m| serde_json::json!({"role": m.role.as_str(), "content": m.content}))
        .collect();

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

        let body = serde_json::json!({
            "model": model,
            "messages": messages,
            "tools": tool_definitions(),
            "tool_choice": "auto",
            "stream": false,
        });
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

        let json: serde_json::Value = resp.json().await?;
        let message = &json["choices"][0]["message"];

        let tool_calls = message["tool_calls"].as_array();
        let has_tools = tool_calls.map(|tc| !tc.is_empty()).unwrap_or(false);

        if !has_tools {
            let content = message["content"].as_str().unwrap_or("").to_string();
            return Ok((content, false, openai_usage(&json["usage"])));
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
            let result = execute_tool(name, &args, workspace, color, caveats).await;
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": id,
                "content": result,
            }));
        }
    }

    // Reached the round cap without a tool-free answer. Make ONE final
    // completion with tools DISABLED (matches the Ollama path).
    final_summary_openai(
        &client,
        &chat_url,
        model,
        api_key,
        &mut messages,
        max_tool_rounds,
    )
    .await
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
                "  /model <name>            — switch model for this session",
                "  /memory                  — show context window / notes usage",
                "  /remember <fact>         — add a fact to persistent NOTES.md",
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
            // List models on the active endpoint, highlighting the current one.
            let choice = resolve_backend_choice();
            let url = choice.url;
            let current = choice.model;
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
                    print_newt(&format!("Models on {url}:"), color, verbose);
                    for name in &names {
                        if *name == current {
                            if color {
                                execute!(
                                    io::stdout(),
                                    Print(format!("  {name}")),
                                    SetForegroundColor(NEWT_ORANGE_CT),
                                    Print(" ◀ active"),
                                    ResetColor,
                                    Print("\n"),
                                )
                                .ok();
                            } else {
                                println!("  {name} ◀ active");
                            }
                        } else {
                            println!("  {name}");
                        }
                    }
                }
                Err(e) => print_newt(&format!("error: {e}"), color, verbose),
            }
        }

        "model" => {
            if arg1.is_empty() {
                let current = resolve_backend_choice().model;
                print_newt(
                    &format!("active model: {current}  (use /model <name> to switch)"),
                    color,
                    verbose,
                );
            } else {
                // Persist via `newt dgx use <model>` then resolve_backend_config
                // picks it up automatically on the next turn.
                run_newt_subcmd(&["dgx", "use", arg1], color, verbose)?;
                print_newt(
                    &format!("Switched to {arg1} — takes effect on next message."),
                    color,
                    verbose,
                );
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
            &caveats,
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
            &caveats,
        )
        .await;
        assert!(
            out.starts_with("capability denied"),
            "an out-of-scope external must be denied via the structured field, got: {out}"
        );
    }

    /// THE test that justifies the change. `echo ok && rm -r <victim>` under a
    /// grant that allows `echo` but NOT `rm`: the `rm` is DENIED inside the
    /// confined shell and the victim file SURVIVES. On the old leading-token +
    /// `sh -c` path the `echo` check passed and `rm` then ran directly, deleting
    /// the victim. Full-command confinement is what stops it here.
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
            &caveats,
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
            &caveats,
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
            &caveats,
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
            &caveats,
        )
        .await;
        assert!(out.contains("one.txt") && out.contains("two.txt"));
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
        let (reply, streamed, _usage) = chat_complete(ChatCtx {
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
        })
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
        let (reply, streamed, _usage) = openai_chat_complete(ChatCtx {
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
        })
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
        let (reply, _streamed, _usage) = chat_complete(ChatCtx {
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
        })
        .await
        .expect("chat_complete should succeed even when final summary errors");

        // Fallback names the limit + the knob — strictly better than the bare
        // placeholder.
        assert!(reply.contains("tool-call limit"));
        assert!(reply.contains("max_tool_rounds"));
    }
}
