//! The branded splash screen — a functional-cohesion extraction from the TUI
//! god-module (#1096, first organizational pass; pure code-motion, non-breaking).
//!
//! Pure display: pick the size-appropriate logo, lay the splash text into a
//! blank band of the art, then wait for a keypress. Leans on the shared
//! brand/logo helpers still in the crate root (`brand_*`, `logo_for_size`).

use std::io::{self, Write};

use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::{
    Color as CtColor, Print, ResetColor, SetBackgroundColor, SetForegroundColor,
};
use crossterm::{queue, terminal};

use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use newt_core::agentic::NEWT_ORANGE_CT;

use crate::{
    brand_active, brand_logo, brand_name, brand_plugins, brand_tagline, logo_for_size, LOGO_PLAIN,
    NEWT_ORANGE, VERSION,
};

fn row_is_blank(row: &str) -> bool {
    const INK: u32 = 56;
    let mut hay = row;
    while let Some(i) = hay.find("8;2;") {
        let mut nums = hay[i + 4..]
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<u32>().unwrap_or(0));
        let r = nums.next().unwrap_or(0);
        let g = nums.next().unwrap_or(0);
        let b = nums.next().unwrap_or(0);
        if r.max(g).max(b) >= INK {
            return false;
        }
        hay = &hay[i + 4..];
    }
    true
}

/// Find a blank band — a run of all-blank rows at the top or bottom of the art
/// — tall enough to hold `need` lines of text. Prefers the bottom (a title-card
/// look under the logo). Returns the `[start, end)` row range to lay text into,
/// or `None` when neither band fits (small logos → caller keeps the side layout).
fn blank_band(rows: &[&str], need: usize) -> Option<(usize, usize)> {
    if need == 0 || rows.is_empty() {
        return None;
    }
    let n = rows.len();
    let mut bottom = n;
    while bottom > 0 && row_is_blank(rows[bottom - 1]) {
        bottom -= 1;
    }
    let bottom_h = n - bottom;
    let mut top = 0;
    while top < n && row_is_blank(rows[top]) {
        top += 1;
    }
    let top_h = top;
    if bottom_h >= need && bottom_h >= top_h {
        Some((bottom, n))
    } else if top_h >= need {
        Some((0, top))
    } else if bottom_h >= need {
        Some((bottom, n))
    } else {
        None
    }
}

/// The splash text block: wordmark + tagline, version, optional plugins, and the
/// action line. Each line is a list of (text, optional fg) spans; `None` fg
/// means the terminal default. Used by the blank-band layout.
fn splash_block() -> Vec<Vec<(String, Option<CtColor>)>> {
    let mut block = vec![
        vec![
            (brand_name(), Some(NEWT_ORANGE_CT)),
            (format!("  ·  {}", brand_tagline()), None),
        ],
        vec![(format!("v{VERSION}"), Some(CtColor::DarkGrey))],
    ];
    if let Some(plugins) = brand_plugins() {
        block.push(vec![(plugins, Some(CtColor::DarkGrey))]);
    }
    block.push(vec![(
        "Enter  start coder   ·   q quit".to_string(),
        Some(CtColor::DarkGrey),
    )]);
    block
}

/// Render the splash. Returns `true` if the user pressed Enter (continue to
/// chat), `false` if they pressed q / Esc / Ctrl-C (quit).
pub(crate) fn show_splash(
    out: &mut io::Stdout,
    workspace: &str,
    color: bool,
    status: &str,
) -> anyhow::Result<bool> {
    if color {
        show_splash_color(out, workspace, status)
    } else {
        show_splash_plain(out, workspace, status)
    }
}

/// One spinner frame from the workspace's ONE braille frame set — the splash
/// always shows motion so a launch never looks hung while background work
/// (model provisioning, backend pre-warm) runs.
fn spinner_frame(tick: u32) -> &'static str {
    let frames = newt_core::tty::SPINNER_FRAMES;
    frames[(tick as usize) % frames.len()]
}

fn show_splash_color(out: &mut io::Stdout, _workspace: &str, status: &str) -> anyhow::Result<bool> {
    let (term_cols, term_rows) = terminal::size().unwrap_or((80, 24));
    let (logo, logo_cols) = logo_for_size(term_cols, term_rows);
    let logo_lines: Vec<&str> = logo.lines().collect();
    let logo_rows = logo_lines.len() as u16;

    // Print ANSI logo flush to top. In raw mode \n is LF only; \r\n resets column.
    write!(out, "{}", logo.replace('\n', "\r\n"))?;
    out.flush()?;

    // A branded logo (e.g. gilamonster's wide half-block hero) leaves big blank
    // bands above/below the subject; lay the splash text into one of them rather
    // than off to the side. Stock newt has no such band → keeps the side layout.
    let block = splash_block();
    if let Some((start, end)) = brand_active()
        .then(|| blank_band(&logo_lines, block.len()))
        .flatten()
    {
        let dark = CtColor::Rgb {
            r: 20,
            g: 20,
            b: 20,
        };
        let top = start + (end - start - block.len()) / 2;
        for (k, line) in block.iter().enumerate() {
            let width: usize = line.iter().map(|(t, _)| t.chars().count()).sum();
            let col = (logo_cols as usize).saturating_sub(width) / 2;
            queue!(
                out,
                MoveTo(col as u16, (top + k) as u16),
                SetBackgroundColor(dark)
            )?;
            for (text, color) in line {
                queue!(out, SetForegroundColor(color.unwrap_or(CtColor::Reset)))?;
                queue!(out, Print(text))?;
            }
            queue!(out, ResetColor)?;
        }
        out.flush()?;
        let spin_row = (top + block.len() + 1).min(logo_lines.len().saturating_sub(1)) as u16;
        let spin_col = (logo_cols as usize).saturating_sub(status.len() + 2) as u16 / 2;
        return splash_wait_with_spinner(out, spin_col, spin_row, status);
    }

    let brand_col = logo_cols + 2;
    let brand_row = logo_rows.saturating_sub(4) / 2;

    let tagline = brand_tagline();
    queue!(out, MoveTo(brand_col, brand_row))?;
    queue!(
        out,
        SetForegroundColor(NEWT_ORANGE_CT),
        Print(brand_name()),
        ResetColor,
        Print(format!("  ·  {tagline}"))
    )?;
    queue!(out, MoveTo(brand_col, brand_row + 1))?;
    queue!(
        out,
        SetForegroundColor(CtColor::DarkGrey),
        Print(format!("v{VERSION}")),
        ResetColor
    )?;
    if let Some(plugins) = brand_plugins() {
        queue!(out, MoveTo(brand_col, brand_row + 2))?;
        queue!(
            out,
            SetForegroundColor(CtColor::DarkGrey),
            Print(plugins),
            ResetColor
        )?;
    }
    queue!(out, MoveTo(brand_col, brand_row + 3))?;
    queue!(
        out,
        SetForegroundColor(CtColor::DarkGrey),
        Print("Enter  start coder   ·   q quit"),
        ResetColor
    )?;
    out.flush()?;

    splash_wait_with_spinner(out, brand_col, brand_row + 5, status)
}

fn show_splash_plain(_out: &mut io::Stdout, workspace: &str, status: &str) -> anyhow::Result<bool> {
    // For the plain path ratatui takes a fresh io::stdout() handle — fine since
    // stdout is a singleton and we already hold raw mode + alt screen.
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut polls: u32 = 0;
    let result = loop {
        terminal.draw(|f| {
            let area = f.area();
            let orange_bold = Style::default()
                .fg(NEWT_ORANGE)
                .add_modifier(Modifier::BOLD);
            let dim = Style::default().fg(Color::DarkGray);
            let mut lines: Vec<Line> = vec![Line::from("")];
            let logo = brand_logo(LOGO_PLAIN, "ascii-40");
            for l in logo.lines() {
                lines.push(Line::from(l.to_owned()));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(brand_name(), orange_bold),
                Span::raw(format!("  ·  {}", brand_tagline())),
            ]));
            lines.push(Line::from(Span::styled(format!("v{VERSION}"), dim)));
            if let Some(plugins) = brand_plugins() {
                lines.push(Line::from(Span::styled(plugins, dim)));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(format!("Workspace:  {workspace}")));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Enter  start coder   ·   q quit",
                dim,
            )));
            lines.push(Line::from(Span::styled(
                format!("{} {status}", spinner_frame(polls)),
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
        polls += 1;
        if polls >= SPLASH_AUTO_CONTINUE_POLLS {
            // ~3 s with no input: auto-continue instead of hanging (#1127).
            break true;
        }
    };
    Ok(result)
}

/// How many 100 ms polls the splash waits for a key before auto-continuing —
/// 30 × 100 ms ≈ 3 s (#1127). The splash used to wait FOREVER with no on-screen
/// hint, so every launch looked hung; now it's a branding beat, not a gate. A
/// keypress still skips (or quits) immediately.
const SPLASH_AUTO_CONTINUE_POLLS: u32 = 30;

/// Poll for a splash keypress. Returns `Some(true)` = continue, `Some(false)` = quit, `None` = keep waiting.
fn splash_poll_event() -> anyhow::Result<Option<bool>> {
    if event::poll(std::time::Duration::from_millis(100))? {
        return Ok(Some(splash_key_action(&event::read()?)));
    }
    Ok(None)
}

/// Wait for the user to press Enter (true) or a quit key (false) — or
/// auto-continue (true) after [`SPLASH_AUTO_CONTINUE_POLLS`] quiet polls —
/// redrawing a spinner + status line each poll so the wait visibly IS a
/// wait, not a hang.
fn splash_wait_with_spinner(
    out: &mut io::Stdout,
    col: u16,
    row: u16,
    status: &str,
) -> anyhow::Result<bool> {
    for tick in 0..SPLASH_AUTO_CONTINUE_POLLS {
        queue!(
            out,
            MoveTo(col, row),
            SetForegroundColor(CtColor::DarkGrey),
            Print(format!("{} {status}", spinner_frame(tick))),
            ResetColor
        )?;
        out.flush()?;
        if event::poll(std::time::Duration::from_millis(100))? {
            return Ok(splash_key_action(&event::read()?));
        }
    }
    Ok(true)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_frames_cycle_through_the_shared_set() {
        // The splash always shows motion (never a hung-looking wait); frames
        // come from the workspace's ONE braille set and wrap cleanly.
        let n = newt_core::tty::SPINNER_FRAMES.len() as u32;
        assert!(n > 1);
        assert_eq!(spinner_frame(0), newt_core::tty::SPINNER_FRAMES[0]);
        assert_eq!(spinner_frame(n), spinner_frame(0), "wraps");
        assert_ne!(spinner_frame(0), spinner_frame(1), "visibly ticks");
    }

    #[test]
    fn row_is_blank_distinguishes_dark_fill_from_ink() {
        // Half-block cell: glyph is always ▄; the picture is in the colors.
        let dark = "\x1b[38;2;20;20;20m\x1b[48;2;18;18;18m▄\x1b[0m";
        let gold = "\x1b[38;2;235;195;70m\x1b[48;2;20;20;20m▄\x1b[0m";
        assert!(row_is_blank(dark), "all-dark row is blank");
        assert!(!row_is_blank(gold), "a gold cell is ink");
        assert!(row_is_blank(""), "empty row is blank");
    }

    #[test]
    fn blank_band_prefers_bottom_and_respects_need() {
        let dark = "\x1b[38;2;20;20;20m\x1b[48;2;20;20;20m▄";
        let ink = "\x1b[38;2;235;195;70m▄";
        // 2 blank rows on top, 3 on the bottom, subject in the middle.
        let rows = [dark, dark, ink, ink, dark, dark, dark];
        assert_eq!(blank_band(&rows, 3), Some((4, 7)), "bottom band fits 3");
        assert_eq!(
            blank_band(&rows, 2),
            Some((4, 7)),
            "bottom preferred over top"
        );
        assert_eq!(blank_band(&rows, 4), None, "neither band holds 4");
        assert_eq!(blank_band(&rows, 0), None);
        // Top-only band when the bottom is too small.
        let top_heavy = [dark, dark, dark, ink, ink];
        assert_eq!(blank_band(&top_heavy, 3), Some((0, 3)));
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
}
