//! The harness **config panel** (issue #14) — behind the `rich-tui` feature.
//!
//! A deliberately-opened, transient ratatui `Viewport::Inline` overlay that lets
//! a human at a TTY adjust the psyche **operator dials** — cognition + tenacity —
//! and returns to the plain scroller. It is **config only**: it renders no agent
//! output, and it WRITES THROUGH THE SAME SETTERS the flags and slash commands
//! use (`set_cli_cognition` / `set_cli_tenacity`), so there is one resolution
//! order and no panel-only state — the swarm runs the identical dial. Per
//! `docs/decisions/harness_config_panel.md` (Accepted 2026-07-28): severable and
//! TTY-only, compiled out of the headless / wyvern tier entirely; no alternate
//! screen (inline region only, mirroring the rich input surface, #416).
//!
//! Crew is shown READ-ONLY: it is a startup gate (`NEWT_TEAM`, the crew runner is
//! built once at launch), so it cannot be toggled live from the panel.
//!
//! The editing/state logic ([`PanelState`]) is pure and unit-tested; the raw-mode
//! event loop ([`run`]) needs a real TTY and is exercised by TUI-drive testing.

use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Terminal, TerminalOptions, Viewport};

use newt_core::cognition::{cli_cognition, set_cli_cognition, CognitionOverride};
use newt_core::role_profile::Cognition;
use newt_core::tenacity::{effective_tenacity, set_cli_tenacity, Tenacity};

type Term = Terminal<CrosstermBackend<Stdout>>;

/// The cognition dial's ladder, in panel order (auto → off → the levels). Cycling
/// left/right steps along this; `apply` installs the selected override verbatim.
const COGNITION_LADDER: &[CognitionOverride] = &[
    CognitionOverride::Unset,
    CognitionOverride::Off,
    CognitionOverride::Set(Cognition::Glancing),
    CognitionOverride::Set(Cognition::Pondering),
    CognitionOverride::Set(Cognition::Deliberating),
    CognitionOverride::Set(Cognition::Contemplating),
];

/// The editable dials, in row order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Cognition,
    Tenacity,
}

const ROWS: [Row; 2] = [Row::Cognition, Row::Tenacity];

/// The panel's working state — a snapshot of the dials the operator edits, seeded
/// from the live values and applied through the canonical setters on close. Pure:
/// no terminal, no I/O, fully unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PanelState {
    sel: usize,
    cognition: CognitionOverride,
    tenacity: Tenacity,
    /// Read-only display of the crew gate (`NEWT_TEAM`); not editable in-session.
    crew_on: bool,
}

impl PanelState {
    /// Seed from the live dials — the same values `/psyche` reports.
    pub(crate) fn from_current() -> Self {
        Self {
            sel: 0,
            cognition: cli_cognition(),
            tenacity: effective_tenacity(),
            crew_on: std::env::var("NEWT_TEAM").is_ok(),
        }
    }

    /// Move the selection down (wraps).
    pub(crate) fn down(&mut self) {
        self.sel = (self.sel + 1) % ROWS.len();
    }

    /// Move the selection up (wraps).
    pub(crate) fn up(&mut self) {
        self.sel = (self.sel + ROWS.len() - 1) % ROWS.len();
    }

    /// Cycle the selected dial's value by `dir` (+1 right / -1 left), clamped to
    /// the ends of its ladder.
    pub(crate) fn cycle(&mut self, dir: i32) {
        match ROWS[self.sel] {
            Row::Cognition => {
                let i = COGNITION_LADDER
                    .iter()
                    .position(|o| *o == self.cognition)
                    .unwrap_or(0);
                let n = COGNITION_LADDER.len() as i32;
                let j = (i as i32 + dir).clamp(0, n - 1) as usize;
                self.cognition = COGNITION_LADDER[j];
            }
            Row::Tenacity => {
                let ladder = Tenacity::all();
                let i = ladder.iter().position(|t| *t == self.tenacity).unwrap_or(0);
                let n = ladder.len() as i32;
                let j = (i as i32 + dir).clamp(0, n - 1) as usize;
                self.tenacity = ladder[j];
            }
        }
    }

    /// Install the edited dials via the canonical process-global setters — the
    /// identical ones the flags and slash commands use.
    pub(crate) fn apply(&self) {
        set_cli_cognition(self.cognition);
        set_cli_tenacity(self.tenacity);
    }

    fn cognition_label(&self) -> &'static str {
        match self.cognition {
            CognitionOverride::Unset => "auto",
            CognitionOverride::Off => "off",
            CognitionOverride::Set(c) => c.label(),
        }
    }

    /// A one-line canonical summary printed into scrollback on close.
    pub(crate) fn summary(&self) -> String {
        format!(
            "psyche · cognition {} · tenacity {} · crew {}",
            self.cognition_label(),
            self.tenacity.label(),
            if self.crew_on { "on" } else { "off" }
        )
    }

    /// The rendered rows: `(label, value, selected, editable)`, in display order.
    fn view_rows(&self) -> Vec<(&'static str, String, bool, bool)> {
        vec![
            (
                "cognition",
                self.cognition_label().to_string(),
                ROWS[self.sel] == Row::Cognition,
                true,
            ),
            (
                "tenacity",
                self.tenacity.label().to_string(),
                ROWS[self.sel] == Row::Tenacity,
                true,
            ),
            (
                "crew",
                format!("{} (launch gate)", if self.crew_on { "on" } else { "off" }),
                false,
                false,
            ),
        ]
    }
}

/// The inline height the panel needs: a bordered block (2) + a title row + the
/// three dial rows + a hint row.
const PANEL_HEIGHT: u16 = 7;

fn make_terminal(height: u16) -> io::Result<Term> {
    Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
}

fn draw(f: &mut ratatui::Frame, state: &PanelState) {
    let area = f.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" psyche — operator dials ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    for (label, value, selected, editable) in state.view_rows() {
        let marker = if selected { "❯ " } else { "  " };
        let name = format!("{marker}{label:<11}");
        let val = if selected && editable {
            format!("‹ {value} ›")
        } else {
            value
        };
        let name_style = if selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if editable {
            Style::default()
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        let val_style = if selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else if editable {
            Style::default()
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        lines.push(Line::from(vec![
            Span::styled(name, name_style),
            Span::styled(val, val_style),
        ]));
    }
    lines.push(Line::from(Span::styled(
        "↑/↓ select · ←/→ change · Enter/Esc apply & close",
        Style::default().add_modifier(Modifier::DIM),
    )));

    let para = Paragraph::new(lines);
    f.render_widget(
        para,
        Rect {
            x: inner.x + 1,
            y: inner.y,
            width: inner.width.saturating_sub(1),
            height: inner.height,
        },
    );
}

/// Open the panel, drive its raw-mode inline event loop until the operator
/// closes it (Enter / Esc / q), apply the edited dials, and return the canonical
/// one-line summary for scrollback. Raw mode is enabled only for the loop's
/// duration and the region is cleared on exit, so subsequent output prints
/// normally (mirrors the rich input surface's `read_turn`).
pub(crate) fn run() -> io::Result<String> {
    let mut state = PanelState::from_current();
    enable_raw_mode()?;
    let loop_result = (|| -> io::Result<()> {
        let mut terminal = make_terminal(PANEL_HEIGHT)?;
        terminal.clear()?;
        loop {
            terminal.draw(|f| draw(f, &state))?;
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Up => state.up(),
                KeyCode::Down => state.down(),
                KeyCode::Left => state.cycle(-1),
                KeyCode::Right => state.cycle(1),
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => break,
                _ => {}
            }
        }
        // Blank the region so the transient overlay leaves no ghost rows in
        // scrollback; the canonical summary is printed by the caller.
        terminal.clear()?;
        Ok(())
    })();
    let _ = disable_raw_mode();
    loop_result?;
    state.apply();
    Ok(state.summary())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_from_live_dials_and_applies_through_the_setters() {
        set_cli_cognition(CognitionOverride::Unset);
        set_cli_tenacity(Tenacity::Standard);
        let mut s = PanelState::from_current();
        assert_eq!(s.cognition, CognitionOverride::Unset);
        assert_eq!(s.tenacity, Tenacity::Standard);
        // Edit both dials, then apply → the canonical globals move.
        s.cycle(1); // cognition auto → off
        s.down(); // select tenacity
        s.cycle(1); // standard → insistent
        s.apply();
        assert_eq!(cli_cognition(), CognitionOverride::Off);
        assert_eq!(effective_tenacity(), Tenacity::Insistent);
        // Restore.
        set_cli_cognition(CognitionOverride::Unset);
        set_cli_tenacity(Tenacity::Standard);
    }

    #[test]
    fn cognition_cycles_the_full_ladder_clamped_at_the_ends() {
        set_cli_cognition(CognitionOverride::Unset);
        set_cli_tenacity(Tenacity::Standard);
        let mut s = PanelState::from_current();
        // Left at the low end stays put (auto).
        s.cycle(-1);
        assert_eq!(s.cognition, CognitionOverride::Unset);
        // Walk right across the whole ladder to the top (contemplating).
        for _ in 0..COGNITION_LADDER.len() {
            s.cycle(1);
        }
        assert_eq!(
            s.cognition,
            CognitionOverride::Set(Cognition::Contemplating)
        );
        // Right at the top stays put.
        s.cycle(1);
        assert_eq!(
            s.cognition,
            CognitionOverride::Set(Cognition::Contemplating)
        );
        set_cli_cognition(CognitionOverride::Unset);
    }

    #[test]
    fn selection_wraps_and_only_the_selected_row_is_editable() {
        let mut s = PanelState::from_current();
        assert_eq!(ROWS[s.sel], Row::Cognition);
        s.up(); // wraps to the last editable row
        assert_eq!(ROWS[s.sel], Row::Tenacity);
        s.down(); // wraps back
        assert_eq!(ROWS[s.sel], Row::Cognition);
        // Crew is present in the view but never editable / selected.
        let crew = s.view_rows().into_iter().find(|r| r.0 == "crew").unwrap();
        assert!(!crew.3, "crew is read-only");
        assert!(crew.1.contains("launch gate"));
    }
}
