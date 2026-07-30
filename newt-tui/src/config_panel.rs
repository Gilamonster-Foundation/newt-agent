//! The harness **config panel** (issue #14) — behind the `rich-tui` feature.
//!
//! A deliberately-opened, transient ratatui `Viewport::Inline` overlay to adjust
//! the psyche **operator dials** — the active persona, cognition, and tenacity —
//! and to SAVE the current posture as a named persona. **Config only** (renders
//! no agent output); applying a dial writes through the same setters the flags /
//! slash commands use, so there is no panel-only state. Per
//! `docs/decisions/harness_config_panel.md` (Accepted 2026-07-28): severable,
//! TTY-only, no alternate screen (inline region, mirroring #416).
//!
//! ## INTERIM SURFACE (review-2 #3)
//! The key handling here is a small, self-contained ex-command reader — it does
//! NOT yet reuse newt's shared vi/emacs editor core (`rich_input` / `vi`). That
//! is deliberate for this first cut; unifying the two onto a shared
//! semantic-command layer (arrow + vi `hjkl`/`gg`/`G` + emacs `C-p/n/b/f`) is
//! tracked as a follow-up. Until then this panel is an **interim overlay**.
//!
//! ## Provenance discipline (review P1#1 / review-2 #1, #2)
//! Dials are seeded from the LIVE override + the EFFECTIVE resolved value, and a
//! dial is written only when the operator changes it. `auto`/`inherit` is a real
//! ladder position that CLEARS the override (cognition → `Unset`, tenacity →
//! cleared) so a value can return to persona/config resolution. SAVE serializes
//! the EFFECTIVE posture (the resolved level), not the raw override.
//!
//! ## Keys (vi-flavoured; save is explicit, Esc always cancels)
//! - `↑`/`↓` select a dial, `←`/`→` change it (incl. `auto`/`inherit`).
//! - `Enter` — apply the changed dials + act on the persona choice, close.
//! - `Esc` / `q` — cancel: discard changes, close (never saves).
//! - `Ctrl-S` or `:w <name>` — save the posture as persona `<name>` (`:w!` to
//!   overwrite). `:wq <name>` — save + apply + close. `:q` — cancel + close.
//!
//! [`PanelState`] is pure and unit-tested; the raw-mode loop ([`run`]) is
//! TUI-drive tested.

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
use newt_core::tenacity::{clear_cli_tenacity, cli_tenacity, set_cli_tenacity, Tenacity};

type Term = Terminal<CrosstermBackend<Stdout>>;

/// The cognition dial's ladder of OVERRIDE positions (auto/inherit → off → levels).
const COGNITION_LADDER: &[CognitionOverride] = &[
    CognitionOverride::Unset, // "auto" — inherit persona/none
    CognitionOverride::Off,
    CognitionOverride::Set(Cognition::Glancing),
    CognitionOverride::Set(Cognition::Pondering),
    CognitionOverride::Set(Cognition::Deliberating),
    CognitionOverride::Set(Cognition::Contemplating),
];

/// The tenacity dial's ladder: `None` = auto/inherit (clear the override), then
/// the concrete levels.
fn tenacity_ladder() -> Vec<Option<Tenacity>> {
    let mut v = vec![None];
    v.extend(Tenacity::all().into_iter().map(Some));
    v
}

/// The "no persona" option shown at index 0 of the persona selector.
const NONE: &str = "none";

/// The editable rows, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Persona,
    Cognition,
    Tenacity,
}

const ROWS: [Row; 3] = [Row::Persona, Row::Cognition, Row::Tenacity];

/// A value the operator may have changed: `Inherit` (untouched — do NOT write) or
/// `Set` (dirty — write on apply).
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

/// What the operator chose to do with the persona selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PersonaAction {
    /// Leave the active persona as-is.
    Keep,
    /// Clear the active persona (`/persona clear`).
    Clear,
    /// Activate this persona.
    Switch(String),
}

/// What the operator did — surfaced to the caller, which owns the session state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PanelOutcome {
    pub summary: String,
    pub persona: PersonaAction,
    pub saved: Option<(String, String)>,
}

/// The panel's working state. Pure: no terminal, no I/O; fully unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PanelState {
    sel: usize,
    /// `NONE` at index 0, then the available persona names.
    persona_opts: Vec<String>,
    persona_idx: usize,
    /// The persona active when the panel opened (for the "(active)" marker + to
    /// tell Keep from Switch/Clear).
    current_persona: Option<String>,
    cognition: Dial<CognitionOverride>,
    /// The EFFECTIVE resolved cognition (override > persona) — shown for `auto`
    /// and serialized on save.
    eff_cognition: Option<Cognition>,
    tenacity: Dial<Option<Tenacity>>,
    /// The EFFECTIVE resolved tenacity — shown for `auto` and serialized on save.
    eff_tenacity: Tenacity,
    crew_on: bool,
    backend: Option<String>,
    mode: Mode,
    /// Transient status / error line (visible feedback for saves + bad commands).
    status: Option<String>,
    saved: Option<(String, String)>,
}

impl PanelState {
    /// Seed from the live overrides + the EFFECTIVE resolved values, the available
    /// personas + the active one, and the current backend name.
    pub(crate) fn new(
        persona_names: Vec<String>,
        current_persona: Option<String>,
        backend: Option<String>,
        eff_cognition: Option<Cognition>,
        eff_tenacity: Tenacity,
    ) -> Self {
        let mut persona_opts = Vec::with_capacity(persona_names.len() + 1);
        persona_opts.push(NONE.to_string());
        persona_opts.extend(persona_names);
        let persona_idx = current_persona
            .as_ref()
            .and_then(|c| persona_opts.iter().position(|n| n == c))
            .unwrap_or(0);
        Self {
            sel: 0,
            persona_opts,
            persona_idx,
            current_persona,
            cognition: Dial::Inherit(cli_cognition()),
            eff_cognition,
            tenacity: Dial::Inherit(cli_tenacity()),
            eff_tenacity,
            crew_on: std::env::var("NEWT_TEAM").is_ok(),
            backend,
            mode: Mode::Normal,
            status: None,
            saved: None,
        }
    }

    fn in_command(&self) -> bool {
        matches!(self.mode, Mode::Command(_))
    }

    pub(crate) fn down(&mut self) {
        self.status = None;
        self.sel = (self.sel + 1) % ROWS.len();
    }
    pub(crate) fn up(&mut self) {
        self.status = None;
        self.sel = (self.sel + ROWS.len() - 1) % ROWS.len();
    }

    pub(crate) fn cycle(&mut self, dir: i32) {
        self.status = None;
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
                let ladder = tenacity_ladder();
                let i = ladder
                    .iter()
                    .position(|t| *t == self.tenacity.value())
                    .unwrap_or(0);
                self.tenacity.set(ladder[clamp_step(i, dir, ladder.len())]);
            }
        }
    }

    /// Apply ONLY the dials the operator changed. `auto`/`inherit` CLEARS the
    /// override so the value returns to persona/config resolution.
    pub(crate) fn apply(&self) {
        if self.cognition.is_dirty() {
            set_cli_cognition(self.cognition.value());
        }
        if self.tenacity.is_dirty() {
            match self.tenacity.value() {
                Some(t) => set_cli_tenacity(t),
                None => clear_cli_tenacity(),
            }
        }
    }

    /// The persona action the operator chose (Keep / Clear / Switch).
    pub(crate) fn persona_action(&self) -> PersonaAction {
        let selected = &self.persona_opts[self.persona_idx];
        if selected == NONE {
            if self.current_persona.is_some() {
                PersonaAction::Clear
            } else {
                PersonaAction::Keep
            }
        } else if Some(selected) == self.current_persona.as_ref() {
            PersonaAction::Keep
        } else {
            PersonaAction::Switch(selected.clone())
        }
    }

    // ── Command line ─────────────────────────────────────────────────────
    fn begin_command(&mut self, prefill: &str) {
        self.status = None;
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

    /// Run the current ex-command. Returns the close intent: `Some(true)` apply +
    /// close, `Some(false)` cancel + close, `None` stay open (with a status line
    /// on error / save confirmation).
    fn run_command(&mut self) -> Option<bool> {
        let cmd = match &self.mode {
            Mode::Command(buf) => buf.trim().to_string(),
            Mode::Normal => return None,
        };
        self.mode = Mode::Normal;
        let mut it = cmd.split_whitespace();
        let raw = it.next().unwrap_or("");
        let overwrite = raw.ends_with('!');
        let verb = raw.trim_end_matches('!');
        let name = it.next();
        match verb {
            "" => None,
            "q" => Some(false),
            "w" => {
                self.try_save(name, overwrite);
                None
            }
            "wq" | "x" => {
                if self.try_save(name, overwrite) {
                    Some(true)
                } else {
                    None // save refused (no name / would overwrite) — stay, show why
                }
            }
            other => {
                self.status = Some(format!("unknown command ':{other}' (:w <name> | :wq | :q)"));
                None
            }
        }
    }

    /// Save the posture as persona `name`. Returns whether it saved; sets a status
    /// line either way (visible feedback — review-2 #5).
    fn try_save(&mut self, name: Option<&str>, overwrite: bool) -> bool {
        let name = sanitize_name(name.unwrap_or(""));
        if name.is_empty() {
            self.status = Some("save needs a name: :w <name>".to_string());
            return false;
        }
        if !overwrite && self.persona_exists(&name) {
            self.status = Some(format!("'{name}' exists — :w! / :wq! to overwrite"));
            return false;
        }
        let content = self.persona_content(&name);
        self.saved = Some((name.clone(), content));
        self.status = Some(format!("saved persona '{name}'"));
        true
    }

    fn persona_exists(&self, name: &str) -> bool {
        self.persona_opts
            .iter()
            .skip(1) // skip NONE
            .any(|n| n.eq_ignore_ascii_case(name))
    }

    /// The EFFECTIVE cognition to serialize on save (auto → the resolved value).
    fn cognition_for_save(&self) -> Option<Cognition> {
        match self.cognition.value() {
            CognitionOverride::Unset => self.eff_cognition,
            CognitionOverride::Off => None,
            CognitionOverride::Set(c) => Some(c),
        }
    }

    /// The EFFECTIVE tenacity to serialize on save (auto → the resolved value).
    fn tenacity_for_save(&self) -> Tenacity {
        self.tenacity.value().unwrap_or(self.eff_tenacity)
    }

    fn persona_content(&self, name: &str) -> String {
        let mut s = String::from("+++\n");
        s.push_str(&format!("role = \"{name}\"\n"));
        if let Some(b) = &self.backend {
            s.push_str(&format!("backend = \"{b}\"\n"));
        }
        if let Some(c) = self.cognition_for_save() {
            s.push_str(&format!("cognition = \"{}\"\n", c.label()));
        }
        s.push_str(&format!(
            "tenacity = \"{}\"\n",
            self.tenacity_for_save().label()
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

    // ── Rendering ────────────────────────────────────────────────────────
    fn cognition_label(&self) -> String {
        match self.cognition.value() {
            CognitionOverride::Unset => format!(
                "auto → {}",
                self.eff_cognition.map_or("off", Cognition::label)
            ),
            CognitionOverride::Off => "off".to_string(),
            CognitionOverride::Set(c) => c.label().to_string(),
        }
    }
    fn tenacity_label(&self) -> String {
        match self.tenacity.value() {
            None => format!("auto → {}", self.eff_tenacity.label()),
            Some(t) => t.label().to_string(),
        }
    }
    fn persona_label(&self) -> String {
        let name = &self.persona_opts[self.persona_idx];
        if Some(name) == self.current_persona.as_ref() {
            format!("{name} (active)")
        } else {
            name.clone()
        }
    }

    pub(crate) fn summary(&self) -> String {
        format!(
            "psyche · persona {} · cognition {} · tenacity {} · crew {}",
            self.persona_opts[self.persona_idx],
            self.cognition_label(),
            self.tenacity_label(),
            if self.crew_on { "on" } else { "off" }
        )
    }

    fn view_rows(&self) -> Vec<(&'static str, String, bool, bool)> {
        vec![
            (
                "persona",
                self.persona_label(),
                ROWS[self.sel] == Row::Persona,
                true,
            ),
            (
                "cognition",
                self.cognition_label(),
                ROWS[self.sel] == Row::Cognition,
                true,
            ),
            (
                "tenacity",
                self.tenacity_label(),
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

fn sanitize_name(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

fn clamp_step(i: usize, dir: i32, len: usize) -> usize {
    let n = len as i32;
    (i as i32 + dir).clamp(0, n - 1) as usize
}

/// Bordered block (2) + four dial rows + a hint/command/status row.
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
    } else if let Some(status) = &state.status {
        Line::from(Span::styled(
            status.clone(),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(Span::styled(
            "↑↓ select · ←→ change (auto=inherit) · Enter apply · Esc cancel · Ctrl-S/:w <name> save",
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
/// Dials apply ONLY on an explicit apply (Enter / `:wq`); Esc / `q` / `:q`
/// discard. Raw mode is enabled only for the loop; the region is cleared on exit.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    persona_names: Vec<String>,
    current_persona: Option<String>,
    backend: Option<String>,
    eff_cognition: Option<Cognition>,
    eff_tenacity: Tenacity,
) -> io::Result<PanelOutcome> {
    let mut state = PanelState::new(
        persona_names,
        current_persona,
        backend,
        eff_cognition,
        eff_tenacity,
    );
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

    let persona = if applied {
        state.persona_action()
    } else {
        PersonaAction::Keep
    };
    if applied {
        state.apply();
    }
    Ok(PanelOutcome {
        summary: state.summary(),
        persona,
        saved: state.saved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::cognition::{effective_cognition, set_persona_cognition};
    use newt_core::tenacity::{effective_tenacity, set_persona_tenacity};
    use newt_core::test_guard::GlobalSettingsGuard;

    fn panel(current: Option<&str>, eff_cog: Option<Cognition>, eff_ten: Tenacity) -> PanelState {
        PanelState::new(
            vec!["bob".to_string(), "obsessive".to_string()],
            current.map(str::to_string),
            Some("sol".to_string()),
            eff_cog,
            eff_ten,
        )
    }

    #[test]
    fn untouched_dials_are_never_written_on_apply() {
        let _g = GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Unset);
        clear_cli_tenacity();
        let s = panel(None, None, Tenacity::Standard);
        s.apply();
        assert_eq!(
            cli_tenacity(),
            None,
            "untouched tenacity stays un-overridden"
        );
        assert_eq!(cli_cognition(), CognitionOverride::Unset);
    }

    #[test]
    fn auto_position_clears_the_tenacity_override() {
        let _g = GlobalSettingsGuard::acquire();
        set_cli_tenacity(Tenacity::Relentless);
        let mut s = panel(None, None, effective_tenacity());
        s.down(); // → cognition
        s.down(); // → tenacity (currently Some(relentless) via cli_tenacity seed)
                  // Cycle left to the `auto` (None) position, then apply → override cleared.
        for _ in 0..tenacity_ladder().len() {
            s.cycle(-1);
        }
        assert_eq!(s.tenacity.value(), None, "reached auto");
        s.apply();
        assert_eq!(cli_tenacity(), None, "auto cleared the override");
    }

    #[test]
    fn save_serializes_the_effective_posture_not_the_raw_override() {
        // review-2 #1: bob declares contemplating; NO operator override. Saving must
        // reproduce contemplating, not drop it because the panel value is `auto`.
        let _g = GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Unset);
        set_persona_cognition(Some(Cognition::Contemplating));
        set_persona_tenacity(Some(Tenacity::Relentless));
        let mut s = panel(Some("bob"), effective_cognition(), effective_tenacity());
        // Nothing touched → save the EFFECTIVE (contemplating / relentless).
        s.begin_command("w clone");
        assert_eq!(s.run_command(), None);
        let (_, content) = s.saved.clone().expect("saved");
        let rp = newt_core::RoleProfile::parse(&content).unwrap();
        assert_eq!(
            rp.cognition,
            Some(Cognition::Contemplating),
            "saved posture preserves effective cognition"
        );
        assert_eq!(rp.tenacity, Some(Tenacity::Relentless));
    }

    #[test]
    fn persona_row_shows_active_and_maps_none_to_clear() {
        let _g = GlobalSettingsGuard::acquire();
        // Active = bob; the selector opens ON bob.
        let mut s = panel(Some("bob"), None, Tenacity::Standard);
        assert_eq!(
            s.persona_action(),
            PersonaAction::Keep,
            "opens on the active"
        );
        assert!(s.persona_label().contains("(active)"));
        // Cycle to `none` → Clear (there is an active persona to clear).
        while s.persona_idx != 0 {
            s.cycle(-1);
        }
        assert_eq!(s.persona_action(), PersonaAction::Clear);
        // Cycle to obsessive → Switch.
        s.cycle(1); // bob
        s.cycle(1); // obsessive
        assert_eq!(
            s.persona_action(),
            PersonaAction::Switch("obsessive".to_string())
        );
    }

    #[test]
    fn ex_commands_validate_visibly() {
        let _g = GlobalSettingsGuard::acquire();
        let mut s = panel(None, None, Tenacity::Standard);
        // :wq with no name → refuse, stay open, visible status.
        s.begin_command("wq");
        assert_eq!(s.run_command(), None, ":wq without a name does not close");
        assert!(s.status.as_deref().unwrap().contains("needs a name"));
        assert!(s.saved.is_none());
        // Unknown command → visible error.
        s.begin_command("banana");
        assert_eq!(s.run_command(), None);
        assert!(s.status.as_deref().unwrap().contains("unknown command"));
        // :w bob (bob exists) → refuse without bang.
        s.begin_command("w bob");
        s.run_command();
        assert!(s.status.as_deref().unwrap().contains("exists"));
        assert!(s.saved.is_none());
        // :w! bob → overwrite allowed.
        s.begin_command("w! bob");
        s.run_command();
        assert!(s.saved.is_some());
        // :wq alice → save + apply + close.
        let mut s2 = panel(None, None, Tenacity::Standard);
        s2.begin_command("wq alice");
        assert_eq!(s2.run_command(), Some(true));
        assert!(s2.saved.is_some());
        // :q → cancel + close.
        s2.begin_command("q");
        assert_eq!(s2.run_command(), Some(false));
    }
}
