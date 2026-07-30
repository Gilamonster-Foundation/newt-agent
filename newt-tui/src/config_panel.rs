//! The harness **config panel** (issue #14) — behind the `rich-tui` feature.
//!
//! A deliberately-opened, transient ratatui `Viewport::Inline` overlay to adjust
//! the psyche **operator dials** — the active persona, cognition, and tenacity —
//! and to SAVE the current posture as a named persona. It is **config only**: it
//! renders no agent output, and applying a dial WRITES THROUGH THE SAME SETTERS
//! the flags and slash commands use, so there is no panel-only state. Per
//! `docs/decisions/harness_config_panel.md` (Accepted 2026-07-28): severable and
//! TTY-only, compiled out of the headless / wyvern tier; no alternate screen
//! (inline region only, mirroring the rich input surface, #416).
//!
//! ## Provenance discipline (review P1#1)
//! A dial is only written when the operator actually **changes** it (dirty), and
//! only on an explicit **apply**. Opening the panel and closing it — or hitting
//! Escape — writes NOTHING, so an untouched, family/config-inherited tenacity is
//! never silently frozen into an explicit override.
//!
//! ## Keys (vi-flavoured; save is explicit, Esc always cancels)
//! - `↑`/`↓` select a dial, `←`/`→` change it (marks it dirty).
//! - `Enter` — **apply** the changed dials + activate the selected persona, close.
//! - `Esc` / `q` — **cancel**: discard all changes, close (never saves).
//! - `Ctrl-S` or `:w <name>` — **save** the current posture as persona `<name>`.
//! - `:wq <name>` — save + apply + close. `:q` — cancel + close.
//!
//! The editing/state logic ([`PanelState`]) is pure and unit-tested; the raw-mode
//! event loop ([`run`]) needs a real TTY and is TUI-drive tested.

use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
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

/// The cognition dial's ladder, in panel order (auto → off → the levels).
const COGNITION_LADDER: &[CognitionOverride] = &[
    CognitionOverride::Unset,
    CognitionOverride::Off,
    CognitionOverride::Set(Cognition::Glancing),
    CognitionOverride::Set(Cognition::Pondering),
    CognitionOverride::Set(Cognition::Deliberating),
    CognitionOverride::Set(Cognition::Contemplating),
];

/// The label shown for the "keep the current persona" option (index 0).
const KEEP: &str = "(keep)";

/// The editable rows, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Persona,
    Cognition,
    Tenacity,
}

const ROWS: [Row; 3] = [Row::Persona, Row::Cognition, Row::Tenacity];

/// A value the operator may have changed: `Inherit` (untouched — do NOT write) or
/// `Set` (dirty — write on apply). This is the review P1#1 fix expressed in the
/// type: an untouched dial can never be persisted as an override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dial<T> {
    Inherit(T),
    Set(T),
}

impl<T: Copy> Dial<T> {
    fn value(self) -> T {
        match self {
            Self::Inherit(v) | Self::Set(v) => v,
        }
    }
    fn is_dirty(self) -> bool {
        matches!(self, Self::Set(_))
    }
    /// Replace the value, marking the dial dirty.
    fn set(&mut self, v: T) {
        *self = Self::Set(v);
    }
}

/// Panel modes: normal navigation, or an ex-command line (`:` / `Ctrl-S`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Normal,
    Command(String),
}

/// What the operator did — surfaced to the caller, which owns the session state.
/// Dials are applied by [`PanelState::apply`] directly (process-globals); persona
/// select + save are returned for the caller to act on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PanelOutcome {
    pub summary: String,
    pub select_persona: Option<String>,
    pub saved: Option<(String, String)>,
}

/// The panel's working state — buffered dials (dirty-tracked) seeded from the live
/// values. Pure: no terminal, no I/O, fully unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PanelState {
    sel: usize,
    persona_opts: Vec<String>,
    persona_idx: usize,
    cognition: Dial<CognitionOverride>,
    tenacity: Dial<Tenacity>,
    crew_on: bool,
    backend: Option<String>,
    mode: Mode,
    saved: Option<(String, String)>,
}

impl PanelState {
    /// Seed from the live dials (the same values `/psyche` reports), the available
    /// persona names, and the current backend name (for save). All dials start
    /// `Inherit` — untouched — so a plain open/close writes nothing.
    pub(crate) fn new(persona_names: Vec<String>, backend: Option<String>) -> Self {
        let mut persona_opts = Vec::with_capacity(persona_names.len() + 1);
        persona_opts.push(KEEP.to_string());
        persona_opts.extend(persona_names);
        Self {
            sel: 0,
            persona_opts,
            persona_idx: 0,
            cognition: Dial::Inherit(cli_cognition()),
            tenacity: Dial::Inherit(effective_tenacity()),
            crew_on: std::env::var("NEWT_TEAM").is_ok(),
            backend,
            mode: Mode::Normal,
            saved: None,
        }
    }

    fn in_command(&self) -> bool {
        matches!(self.mode, Mode::Command(_))
    }

    pub(crate) fn down(&mut self) {
        self.sel = (self.sel + 1) % ROWS.len();
    }
    pub(crate) fn up(&mut self) {
        self.sel = (self.sel + ROWS.len() - 1) % ROWS.len();
    }

    /// Cycle the selected dial by `dir` (+1 right / -1 left), clamped, marking it
    /// dirty.
    pub(crate) fn cycle(&mut self, dir: i32) {
        match ROWS[self.sel] {
            Row::Persona => {
                self.persona_idx = clamp_step(self.persona_idx, dir, self.persona_opts.len());
            }
            Row::Cognition => {
                let i = COGNITION_LADDER
                    .iter()
                    .position(|o| *o == self.cognition.value())
                    .unwrap_or(0);
                self.cognition
                    .set(COGNITION_LADDER[clamp_step(i, dir, COGNITION_LADDER.len())]);
            }
            Row::Tenacity => {
                let ladder = Tenacity::all();
                let i = ladder
                    .iter()
                    .position(|t| *t == self.tenacity.value())
                    .unwrap_or(0);
                self.tenacity.set(ladder[clamp_step(i, dir, ladder.len())]);
            }
        }
    }

    /// Apply ONLY the dials the operator changed (dirty) — untouched dials keep
    /// their inherited resolution. Called on an explicit apply, never on cancel.
    pub(crate) fn apply(&self) {
        if self.cognition.is_dirty() {
            set_cli_cognition(self.cognition.value());
        }
        if self.tenacity.is_dirty() {
            set_cli_tenacity(self.tenacity.value());
        }
    }

    /// The persona the operator chose to activate (index 0 = keep = `None`).
    pub(crate) fn selected_persona(&self) -> Option<String> {
        (self.persona_idx > 0).then(|| self.persona_opts[self.persona_idx].clone())
    }

    // ── Command-line handling ────────────────────────────────────────────
    fn begin_command(&mut self, prefill: &str) {
        self.mode = Mode::Command(prefill.to_string());
    }
    fn command_char(&mut self, c: char) {
        if let Mode::Command(buf) = &mut self.mode {
            buf.push(c);
        }
    }
    fn command_backspace(&mut self) {
        if let Mode::Command(buf) = &mut self.mode {
            buf.pop();
        }
    }
    fn cancel_command(&mut self) {
        self.mode = Mode::Normal;
    }

    /// Run the current ex-command. Returns the close intent: `Some(true)` = close
    /// applying, `Some(false)` = close discarding, `None` = stay open.
    fn run_command(&mut self) -> Option<bool> {
        let cmd = match &self.mode {
            Mode::Command(buf) => buf.trim().to_string(),
            Mode::Normal => return None,
        };
        self.mode = Mode::Normal;
        let mut it = cmd.split_whitespace();
        match it.next() {
            Some("q") | Some("q!") => Some(false), // quit, discard
            Some("w") => {
                self.save_as(it.next());
                None // saved; stay open
            }
            Some("wq") | Some("x") => {
                self.save_as(it.next());
                Some(true) // saved; apply + close
            }
            _ => None, // unknown — ignore, stay open
        }
    }

    /// Save the current posture as a persona named `name` (a valid, sanitized
    /// stem); no-op if the name is empty/absent. Stashes `(name, content)` for the
    /// caller to write.
    fn save_as(&mut self, name: Option<&str>) {
        let name = sanitize_name(name.unwrap_or(""));
        if !name.is_empty() {
            let content = self.persona_content(&name);
            self.saved = Some((name, content));
        }
    }

    /// The persona-file content for the current posture. Cognition is pinned only
    /// when it is an explicit level (auto/off don't map to a value); crew only
    /// when on.
    fn persona_content(&self, name: &str) -> String {
        let mut s = String::from("+++\n");
        s.push_str(&format!("role = \"{name}\"\n"));
        if let Some(b) = &self.backend {
            s.push_str(&format!("backend = \"{b}\"\n"));
        }
        if let CognitionOverride::Set(c) = self.cognition.value() {
            s.push_str(&format!("cognition = \"{}\"\n", c.label()));
        }
        s.push_str(&format!(
            "tenacity = \"{}\"\n",
            self.tenacity.value().label()
        ));
        if self.crew_on {
            s.push_str("crew = true\n");
        }
        s.push_str("+++\n\n");
        s.push_str(&format!(
            "# {name}\n\nSaved from the psyche panel — the dials above define this persona's posture.\n"
        ));
        s
    }

    fn cognition_label(&self) -> &'static str {
        match self.cognition.value() {
            CognitionOverride::Unset => "auto",
            CognitionOverride::Off => "off",
            CognitionOverride::Set(c) => c.label(),
        }
    }
    fn persona_label(&self) -> &str {
        &self.persona_opts[self.persona_idx]
    }

    /// A one-line canonical summary printed into scrollback on close.
    pub(crate) fn summary(&self) -> String {
        format!(
            "psyche · persona {} · cognition {} · tenacity {} · crew {}",
            self.persona_label(),
            self.cognition_label(),
            self.tenacity.value().label(),
            if self.crew_on { "on" } else { "off" }
        )
    }

    /// `(label, value, selected, editable)` per row, in display order.
    fn view_rows(&self) -> Vec<(&'static str, String, bool, bool)> {
        vec![
            (
                "persona",
                self.persona_label().to_string(),
                ROWS[self.sel] == Row::Persona,
                true,
            ),
            (
                "cognition",
                self.cognition_label().to_string(),
                ROWS[self.sel] == Row::Cognition,
                true,
            ),
            (
                "tenacity",
                self.tenacity.value().label().to_string(),
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

/// Keep only file-stem-safe characters for a persona name.
fn sanitize_name(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

/// Step `i` by `dir` within `0..len`, clamped to the ends.
fn clamp_step(i: usize, dir: i32, len: usize) -> usize {
    let n = len as i32;
    (i as i32 + dir).clamp(0, n - 1) as usize
}

/// The inline height: a bordered block (2) + the four dial rows + a hint/command row.
const PANEL_HEIGHT: u16 = 8;

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
        let (name_style, val_style) = row_styles(selected, editable);
        lines.push(Line::from(vec![
            Span::styled(name, name_style),
            Span::styled(val, val_style),
        ]));
    }
    let bottom = if let Mode::Command(buf) = &state.mode {
        Line::from(Span::styled(
            format!(":{buf}▏"),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(Span::styled(
            "↑↓ select · ←→ change · Enter apply · Esc cancel · Ctrl-S/:w <name> save",
            Style::default().add_modifier(Modifier::DIM),
        ))
    };
    lines.push(bottom);

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

fn row_styles(selected: bool, editable: bool) -> (Style, Style) {
    if selected {
        (
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else if editable {
        (Style::default(), Style::default())
    } else {
        (
            Style::default().add_modifier(Modifier::DIM),
            Style::default().add_modifier(Modifier::DIM),
        )
    }
}

/// Open the panel, drive its raw-mode inline event loop, and return the outcome.
/// Dials are applied ONLY on an explicit apply (Enter / `:wq`); Esc / `q` / `:q`
/// discard. Raw mode is enabled only for the loop's duration and the region is
/// cleared on exit.
pub(crate) fn run(persona_names: Vec<String>, backend: Option<String>) -> io::Result<PanelOutcome> {
    let mut state = PanelState::new(persona_names, backend);
    let mut applied = false;
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
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            if state.in_command() {
                match key.code {
                    KeyCode::Char(c) => state.command_char(c),
                    KeyCode::Backspace => state.command_backspace(),
                    KeyCode::Esc => state.cancel_command(),
                    KeyCode::Enter => {
                        if let Some(apply) = state.run_command() {
                            applied = apply;
                            break;
                        }
                    }
                    _ => {}
                }
            } else {
                match key.code {
                    KeyCode::Up => state.up(),
                    KeyCode::Down => state.down(),
                    KeyCode::Left => state.cycle(-1),
                    KeyCode::Right => state.cycle(1),
                    // Ctrl-S → save: open the command line pre-filled with `w `.
                    KeyCode::Char('s') if ctrl => state.begin_command("w "),
                    KeyCode::Char(':') => state.begin_command(""),
                    KeyCode::Enter => {
                        applied = true;
                        break;
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        applied = false;
                        break;
                    }
                    _ => {}
                }
            }
        }
        terminal.clear()?;
        Ok(())
    })();
    let _ = disable_raw_mode();
    loop_result?;

    if applied {
        state.apply();
    }
    Ok(PanelOutcome {
        summary: state.summary(),
        select_persona: applied.then(|| state.selected_persona()).flatten(),
        saved: state.saved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::tenacity::{clear_cli_tenacity, cli_tenacity};
    use newt_core::test_guard::GlobalSettingsGuard;

    fn panel() -> PanelState {
        PanelState::new(
            vec!["bob".to_string(), "obsessive".to_string()],
            Some("sol".to_string()),
        )
    }

    #[test]
    fn untouched_dials_are_never_written_on_apply() {
        // Review P1#1: opening and applying without editing must NOT freeze the
        // inherited tenacity into an explicit override.
        let _g = GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Unset);
        clear_cli_tenacity(); // no explicit override → family/config resolution
        assert_eq!(cli_tenacity(), None);

        let s = panel();
        s.apply(); // nothing cycled → nothing dirty → nothing written
        assert_eq!(
            cli_tenacity(),
            None,
            "untouched tenacity stays un-overridden"
        );
        assert_eq!(cli_cognition(), CognitionOverride::Unset);
    }

    #[test]
    fn only_changed_dials_are_applied() {
        let _g = GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Unset);
        clear_cli_tenacity();
        let mut s = panel();
        s.down(); // → cognition
        s.cycle(1); // auto → off (dirty)
        s.apply();
        assert_eq!(
            cli_cognition(),
            CognitionOverride::Off,
            "changed dial applied"
        );
        assert_eq!(cli_tenacity(), None, "untouched tenacity not written");
    }

    #[test]
    fn persona_row_selects_a_name_and_keep_is_none() {
        let _g = GlobalSettingsGuard::acquire();
        let mut s = panel();
        assert!(s.selected_persona().is_none());
        s.cycle(1); // keep → bob
        assert_eq!(s.selected_persona().as_deref(), Some("bob"));
        s.cycle(1); // bob → obsessive
        assert_eq!(s.selected_persona().as_deref(), Some("obsessive"));
        s.cycle(1); // clamp
        assert_eq!(s.selected_persona().as_deref(), Some("obsessive"));
    }

    #[test]
    fn save_command_builds_a_valid_persona_file() {
        let _g = GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Unset);
        clear_cli_tenacity();
        let mut s = panel();
        s.down(); // → cognition
        for _ in 0..COGNITION_LADDER.len() {
            s.cycle(1); // → contemplating
        }
        s.down(); // → tenacity
        for _ in 0..Tenacity::all().len() {
            s.cycle(1); // → relentless
        }
        s.begin_command("w alice");
        assert_eq!(s.run_command(), None, ":w saves and stays open");
        let (name, content) = s.saved.clone().expect("saved");
        assert_eq!(name, "alice");
        let rp = newt_core::RoleProfile::parse(&content).expect("valid persona");
        assert_eq!(rp.backend.as_deref(), Some("sol"));
        assert_eq!(rp.cognition, Some(Cognition::Contemplating));
        assert_eq!(rp.tenacity, Some(Tenacity::Relentless));
    }

    #[test]
    fn wq_saves_and_applies_q_discards() {
        let _g = GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Unset);
        // :q → close, discard (intent = Some(false)).
        let mut s = panel();
        s.down();
        s.cycle(1); // dirty cognition
        s.begin_command("q");
        assert_eq!(s.run_command(), Some(false), ":q discards + closes");

        // :wq bob → save + apply (intent = Some(true)).
        let mut s2 = panel();
        s2.down();
        s2.cycle(1);
        s2.begin_command("wq bob");
        assert_eq!(s2.run_command(), Some(true), ":wq saves + applies + closes");
        assert!(s2.saved.is_some());
    }
}
