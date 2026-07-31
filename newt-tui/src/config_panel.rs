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
//! tracked as #1495. Until then this panel is an **interim overlay**.
//!
//! ## Transaction semantics (review-3)
//! Editing → saving → applying → cancelling → reporting are explicit, ordered,
//! and internally consistent — the panel never leaves the runtime partially
//! modified or reports abandoned edits as active:
//!
//! - **Save is I/O-injected** ([`run`] takes a `persist` closure) so the file
//!   write happens *inside* the event loop and can keep the panel open on failure.
//!   The panel shows a status from the returned [`SaveResult`] and only records
//!   success once the write actually succeeded.
//! - **`:wq` commit order** — validate → persist the persona file → apply
//!   cognition/tenacity → (caller) apply persona + reroute backend → (caller)
//!   recompute + report. A failed persist returns `None` from the command, so the
//!   loop never breaks and [`PanelState::apply`] never runs: no dial, persona, or
//!   backend change happens.
//! - **[`PanelOutcome`] distinguishes cancellation from application.** On
//!   cancel the caller prints "cancelled" (or nothing) — never a posture summary
//!   from the abandoned working copy. After an apply the caller builds the summary
//!   from freshly-resolved runtime state, not from panel-local values.
//! - **Persona preview is projected, not stale** (review-3 §3). Selecting a
//!   persona recomputes the projected effective cognition / tenacity / backend /
//!   crew from *that* persona's declarations (over the config/family base), with a
//!   provenance label distinguishing an explicit override from an inherited value.
//!   Save serializes exactly this projected posture.
//!
//! ## Keys (vi-flavoured; save is explicit, Esc always cancels)
//! - `↑`/`↓` select a dial, `←`/`→` change it (incl. `auto`/`inherit`).
//! - `Enter` — apply the changed dials + act on the persona choice, close.
//! - `Esc` / `q` — cancel: discard changes, close (never applies).
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

/// A persona the panel can select, with the declarations needed to PROJECT its
/// effective posture (review-3 §3). Built by the caller from each persona's role
/// profile; `None` fields mean the persona inherits that dial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersonaChoice {
    pub name: String,
    pub cognition: Option<Cognition>,
    pub tenacity: Option<Tenacity>,
    pub backend: Option<String>,
    pub crew: Option<bool>,
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

/// The result of a single save attempt — the panel shows a status from this and
/// records success ONLY when the filesystem write actually succeeded (review-3
/// §1). Produced by the caller's `persist` closure (which owns the `PersonaStore`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SaveResult {
    /// The persona file was written.
    Saved { name: String },
    /// A persona `name` already exists and `!`-overwrite was not requested.
    Exists { name: String },
    /// The name was empty / not a valid file stem.
    InvalidName(String),
    /// The write itself failed (I/O error text).
    Failed(String),
}

/// What the panel returned — cancellation is distinguishable from application
/// (review-3 §2), and there is deliberately NO summary string: the caller reports
/// from freshly-resolved runtime state after committing, never from the panel's
/// (possibly abandoned) working copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PanelOutcome {
    /// Esc / `q` / `:q`: discard everything. Nothing was applied.
    Cancelled,
    /// Enter / `:wq`-with-no-save: dials were applied; act on `persona`.
    Applied { persona: PersonaAction },
    /// `:w` then cancel: the file was persisted, but dials were NOT applied.
    Saved { name: String },
    /// `:wq`: the file was persisted AND dials were applied; act on `persona`.
    SavedAndApplied {
        name: String,
        persona: PersonaAction,
    },
}

/// The panel's working state. Pure: no terminal, no I/O; fully unit-testable.
/// Persistence is injected into [`PanelState::run_command`] as a closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PanelState {
    sel: usize,
    /// `NONE` at index 0, then the available persona names (parallel to
    /// [`Self::personas`] shifted by one).
    persona_opts: Vec<String>,
    /// The selectable personas with their declarations, for projection.
    personas: Vec<PersonaChoice>,
    persona_idx: usize,
    /// The persona active when the panel opened (for the "(active)" marker + to
    /// tell Keep from Switch/Clear).
    current_persona: Option<String>,
    cognition: Dial<CognitionOverride>,
    tenacity: Dial<Option<Tenacity>>,
    /// Tenacity with no CLI override and no persona layer (config per-family /
    /// default / `Standard`) — the value a persona that declares none inherits.
    base_tenacity: Tenacity,
    /// The crew launch gate at open (`NEWT_TEAM`) — the base a persona's `crew:`
    /// declaration projects over.
    base_crew: bool,
    /// The backend active at open — the projection fallback + save default.
    backend: Option<String>,
    mode: Mode,
    /// Transient status / error line (visible feedback for saves + bad commands).
    status: Option<String>,
    /// The name of the most recent SUCCESSFUL save this session (`None` until a
    /// `:w`/`:wq` write actually lands). Distinct from a proposed edit.
    saved: Option<String>,
}

impl PanelState {
    /// Seed from the live overrides, the selectable personas + the active one, the
    /// current backend, and the config/family tenacity base (for projection).
    pub(crate) fn new(
        personas: Vec<PersonaChoice>,
        current_persona: Option<String>,
        backend: Option<String>,
        base_tenacity: Tenacity,
    ) -> Self {
        let mut persona_opts = Vec::with_capacity(personas.len() + 1);
        persona_opts.push(NONE.to_string());
        persona_opts.extend(personas.iter().map(|p| p.name.clone()));
        let persona_idx = current_persona
            .as_ref()
            .and_then(|c| persona_opts.iter().position(|n| n == c))
            .unwrap_or(0);
        Self {
            sel: 0,
            persona_opts,
            personas,
            persona_idx,
            current_persona,
            cognition: Dial::Inherit(cli_cognition()),
            tenacity: Dial::Inherit(cli_tenacity()),
            base_tenacity,
            base_crew: std::env::var("NEWT_TEAM").is_ok(),
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
    /// override so the value returns to persona/config resolution. Called by
    /// [`run`] only after an explicit apply (Enter / a `:wq` whose save landed).
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

    // ── Projection (review-3 §3) ─────────────────────────────────────────
    /// The persona currently highlighted in the selector (`None` = the NONE row).
    fn selected_persona(&self) -> Option<&PersonaChoice> {
        if self.persona_idx == 0 {
            None
        } else {
            self.personas.get(self.persona_idx - 1)
        }
    }

    /// The cognition that WILL be in effect for the selected persona after Apply:
    /// an explicit override wins, else the selected persona's declared level, else
    /// none.
    fn projected_cognition(&self) -> Option<Cognition> {
        match self.cognition.value() {
            CognitionOverride::Set(c) => Some(c),
            CognitionOverride::Off => None,
            CognitionOverride::Unset => self.selected_persona().and_then(|p| p.cognition),
        }
    }

    /// The tenacity that WILL be in effect for the selected persona after Apply: an
    /// explicit override wins, else the selected persona's declared level, else the
    /// config/family base.
    fn projected_tenacity(&self) -> Tenacity {
        match self.tenacity.value() {
            Some(t) => t,
            None => self
                .selected_persona()
                .and_then(|p| p.tenacity)
                .unwrap_or(self.base_tenacity),
        }
    }

    /// The backend that WILL be in effect: the selected persona's declared backend,
    /// else the current one.
    fn projected_backend(&self) -> Option<String> {
        self.selected_persona()
            .and_then(|p| p.backend.clone())
            .or_else(|| self.backend.clone())
    }

    /// The crew launch gate that the selected persona declares, else the base.
    fn projected_crew(&self) -> bool {
        self.selected_persona()
            .and_then(|p| p.crew)
            .unwrap_or(self.base_crew)
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

    /// Run the current ex-command, using `persist` for any file write. Returns the
    /// close intent: `Some(true)` apply + close, `Some(false)` cancel + close,
    /// `None` stay open (with a status line on error / save confirmation). The
    /// `persist` closure is the ONLY I/O — injected so this is unit-testable and so
    /// a failed write keeps the panel open with edits intact (review-3 §1).
    fn run_command(
        &mut self,
        persist: &mut dyn FnMut(&str, &str, bool) -> SaveResult,
    ) -> Option<bool> {
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
                self.try_save(name, overwrite, persist);
                None
            }
            "wq" | "x" => {
                if self.try_save(name, overwrite, persist) {
                    Some(true)
                } else {
                    None // save refused (no name / exists / failed) — stay, show why
                }
            }
            other => {
                self.status = Some(format!("unknown command ':{other}' (:w <name> | :wq | :q)"));
                None
            }
        }
    }

    /// Persist the projected posture as persona `name` via `persist`. Returns
    /// whether it saved; sets a status line from the [`SaveResult`] either way
    /// (visible feedback — review-2 #5 / review-3 §1). Nothing is recorded as saved
    /// until the write actually succeeds.
    fn try_save(
        &mut self,
        name: Option<&str>,
        overwrite: bool,
        persist: &mut dyn FnMut(&str, &str, bool) -> SaveResult,
    ) -> bool {
        let name = sanitize_name(name.unwrap_or(""));
        if name.is_empty() {
            self.status = Some("save needs a name: :w <name>".to_string());
            return false;
        }
        let content = self.persona_content(&name);
        match persist(&name, &content, overwrite) {
            SaveResult::Saved { name } => {
                self.status = Some(format!("saved persona '{name}'"));
                self.saved = Some(name);
                true
            }
            SaveResult::Exists { name } => {
                self.status = Some(format!("'{name}' exists — :w! / :wq! to overwrite"));
                false
            }
            SaveResult::InvalidName(msg) => {
                self.status = Some(format!("invalid name: {msg}"));
                false
            }
            SaveResult::Failed(err) => {
                self.status = Some(format!("save failed: {err}"));
                false
            }
        }
    }

    /// The EFFECTIVE cognition to serialize on save — the PROJECTED value for the
    /// selected persona (auto → the persona's declared level, or none).
    fn cognition_for_save(&self) -> Option<Cognition> {
        self.projected_cognition()
    }

    /// The EFFECTIVE tenacity to serialize on save — the PROJECTED value for the
    /// selected persona (auto → the persona's declared level, or the base).
    fn tenacity_for_save(&self) -> Tenacity {
        self.projected_tenacity()
    }

    fn persona_content(&self, name: &str) -> String {
        let mut s = String::from("+++\n");
        s.push_str(&format!("role = \"{name}\"\n"));
        if let Some(b) = self.projected_backend() {
            s.push_str(&format!("backend = \"{b}\"\n"));
        }
        if let Some(c) = self.cognition_for_save() {
            s.push_str(&format!("cognition = \"{}\"\n", c.label()));
        }
        s.push_str(&format!(
            "tenacity = \"{}\"\n",
            self.tenacity_for_save().label()
        ));
        if self.projected_crew() {
            s.push_str("crew = true\n");
        }
        s.push_str("+++\n\n");
        s.push_str(&format!(
            "# {name}\n\nSaved from the psyche panel — the dials above define this persona's posture.\n"
        ));
        s
    }

    // ── Rendering ────────────────────────────────────────────────────────
    /// `(value, provenance)` for the cognition row. Provenance distinguishes an
    /// explicit override from a value inherited from the selected persona / base.
    fn cognition_cell(&self) -> (String, String) {
        match self.cognition.value() {
            CognitionOverride::Set(c) => (c.label().to_string(), "override".to_string()),
            CognitionOverride::Off => ("off".to_string(), "override".to_string()),
            CognitionOverride::Unset => {
                let proj = self.projected_cognition();
                let val = format!("auto → {}", proj.map_or("off", Cognition::label));
                (val, self.inherit_provenance(proj.is_some()))
            }
        }
    }
    fn tenacity_cell(&self) -> (String, String) {
        match self.tenacity.value() {
            Some(t) => (t.label().to_string(), "override".to_string()),
            None => {
                let val = format!("auto → {}", self.projected_tenacity().label());
                let from_persona = self.selected_persona().and_then(|p| p.tenacity).is_some();
                (val, self.inherit_provenance(from_persona))
            }
        }
    }
    /// Provenance for an inherited (non-override) value: attribute it to the
    /// selected persona when that persona declares the dial, else the base.
    fn inherit_provenance(&self, from_persona: bool) -> String {
        match (from_persona, self.selected_persona()) {
            (true, Some(p)) => format!("persona: {}", p.name),
            _ => "base".to_string(),
        }
    }
    fn persona_label(&self) -> String {
        let name = &self.persona_opts[self.persona_idx];
        if Some(name) == self.current_persona.as_ref() {
            format!("{name} (active)")
        } else if name == NONE {
            if self.current_persona.is_some() {
                "none (clears active)".to_string()
            } else {
                "none".to_string()
            }
        } else {
            format!("{name} (pending)")
        }
    }

    fn view_rows(&self) -> Vec<RowView> {
        let (cog_val, cog_prov) = self.cognition_cell();
        let (ten_val, ten_prov) = self.tenacity_cell();
        vec![
            RowView {
                label: "persona",
                value: self.persona_label(),
                provenance: String::new(),
                selected: ROWS[self.sel] == Row::Persona,
                editable: true,
            },
            RowView {
                label: "cognition",
                value: cog_val,
                provenance: cog_prov,
                selected: ROWS[self.sel] == Row::Cognition,
                editable: true,
            },
            RowView {
                label: "tenacity",
                value: ten_val,
                provenance: ten_prov,
                selected: ROWS[self.sel] == Row::Tenacity,
                editable: true,
            },
            RowView {
                label: "provider",
                value: self.projected_backend().unwrap_or_else(|| "—".to_string()),
                provenance: self.backend_provenance(),
                selected: false,
                editable: false,
            },
            RowView {
                label: "crew",
                value: format!(
                    "{} (launch gate)",
                    if self.projected_crew() { "on" } else { "off" }
                ),
                provenance: self.crew_provenance(),
                selected: false,
                editable: false,
            },
        ]
    }

    fn backend_provenance(&self) -> String {
        match self.selected_persona() {
            Some(p) if p.backend.is_some() => format!("persona: {}", p.name),
            _ => "current".to_string(),
        }
    }
    fn crew_provenance(&self) -> String {
        match self.selected_persona() {
            Some(p) if p.crew.is_some() => {
                format!("declaration: {}; applies next launch", p.name)
            }
            _ => "current launch gate".to_string(),
        }
    }
}

/// A rendered row: label, value, provenance, and display flags.
struct RowView {
    label: &'static str,
    value: String,
    provenance: String,
    selected: bool,
    editable: bool,
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

/// Bordered block (2) + five rows + a hint/command/status row.
const PANEL_HEIGHT: u16 = 9;

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
    for row in state.view_rows() {
        let marker = if row.selected { "❯ " } else { "  " };
        let name = format!("{marker}{:<11}", row.label);
        let val = if row.selected && row.editable {
            format!("‹ {} ›", row.value)
        } else {
            row.value.clone()
        };
        let (name_style, val_style) = row_styles(row.selected, row.editable);
        let mut spans = vec![
            Span::styled(name, name_style),
            Span::styled(format!("{val:<26}"), val_style),
        ];
        if !row.provenance.is_empty() {
            spans.push(Span::styled(
                row.provenance,
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        lines.push(Line::from(spans));
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
/// `persist` writes a persona file and reports the [`SaveResult`] — it is called
/// during `:w`/`:wq` BEFORE any dial is applied, so a failed write leaves the
/// runtime untouched and the panel open (review-3 §1). Dials apply ONLY on an
/// explicit apply (Enter / a `:wq` whose save landed); Esc / `q` / `:q` discard.
pub(crate) fn run(
    personas: Vec<PersonaChoice>,
    current_persona: Option<String>,
    backend: Option<String>,
    base_tenacity: Tenacity,
    mut persist: impl FnMut(&str, &str, bool) -> SaveResult,
) -> io::Result<PanelOutcome> {
    let mut state = PanelState::new(personas, current_persona, backend, base_tenacity);
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
                        if let Some(apply) = state.run_command(&mut persist) {
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

    // Commit order (review-3 §1): the persona file was already persisted inside
    // the loop (via `persist`). Now apply the dials — but ONLY on an explicit
    // apply — then hand the persona action to the caller, which applies it, reroutes
    // the backend, recomputes, and reports from fresh runtime state.
    Ok(if applied {
        state.apply();
        let persona = state.persona_action();
        match state.saved {
            Some(name) => PanelOutcome::SavedAndApplied { name, persona },
            None => PanelOutcome::Applied { persona },
        }
    } else {
        match state.saved {
            Some(name) => PanelOutcome::Saved { name },
            None => PanelOutcome::Cancelled,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::test_guard::GlobalSettingsGuard;

    fn choice(
        name: &str,
        cognition: Option<Cognition>,
        tenacity: Option<Tenacity>,
    ) -> PersonaChoice {
        PersonaChoice {
            name: name.to_string(),
            cognition,
            tenacity,
            backend: Some("sol".to_string()),
            crew: None,
        }
    }

    fn panel(
        current: Option<&str>,
        personas: Vec<PersonaChoice>,
        base_ten: Tenacity,
    ) -> PanelState {
        PanelState::new(
            personas,
            current.map(str::to_string),
            Some("sol".to_string()),
            base_ten,
        )
    }

    fn two_personas() -> Vec<PersonaChoice> {
        vec![choice("bob", None, None), choice("obsessive", None, None)]
    }

    /// A `persist` closure that always succeeds (records nothing to disk).
    fn ok_persist() -> impl FnMut(&str, &str, bool) -> SaveResult {
        |name: &str, _content: &str, _overwrite: bool| SaveResult::Saved {
            name: name.to_string(),
        }
    }

    #[test]
    fn untouched_dials_are_never_written_on_apply() {
        let _g = GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Unset);
        clear_cli_tenacity();
        let s = panel(None, two_personas(), Tenacity::Standard);
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
        let mut s = panel(None, two_personas(), Tenacity::Standard);
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
    fn projection_follows_the_selected_persona() {
        // review-3 §3: selecting a persona projects ITS declarations, never the
        // previously-active persona's effective values.
        let _g = GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Unset);
        clear_cli_tenacity();
        let personas = vec![
            choice("bob", Some(Cognition::Pondering), Some(Tenacity::Standard)),
            choice(
                "obsessive",
                Some(Cognition::Contemplating),
                Some(Tenacity::Relentless),
            ),
        ];
        let mut s = panel(Some("bob"), personas, Tenacity::Standard);
        // Opens on bob → projects bob's declarations.
        assert_eq!(s.projected_cognition(), Some(Cognition::Pondering));
        assert_eq!(s.projected_tenacity(), Tenacity::Standard);
        // Select obsessive → projection switches to obsessive's, not bob's.
        s.cycle(1); // bob
        s.cycle(1); // obsessive
        assert_eq!(s.projected_cognition(), Some(Cognition::Contemplating));
        assert_eq!(s.projected_tenacity(), Tenacity::Relentless);
        let (ten_val, ten_prov) = s.tenacity_cell();
        assert!(ten_val.contains("relentless"), "shows projected level");
        assert_eq!(ten_prov, "persona: obsessive", "attributes it to obsessive");
    }

    #[test]
    fn projection_base_shows_when_the_persona_declares_nothing() {
        let _g = GlobalSettingsGuard::acquire();
        clear_cli_tenacity();
        // obsessive declares no tenacity → inherits the config/family base.
        let personas = vec![choice("obsessive", None, None)];
        let mut s = panel(None, personas, Tenacity::Insistent);
        s.cycle(1); // NONE → obsessive
        assert_eq!(s.projected_tenacity(), Tenacity::Insistent, "inherits base");
        let (_, prov) = s.tenacity_cell();
        assert_eq!(prov, "base");
    }

    #[test]
    fn save_serializes_the_projected_posture() {
        // review-2 #1 / review-3 §3: saving reproduces the PROJECTED effective
        // posture of the selected persona, not an empty field.
        let _g = GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Unset);
        clear_cli_tenacity();
        let personas = vec![choice(
            "bob",
            Some(Cognition::Contemplating),
            Some(Tenacity::Relentless),
        )];
        let mut s = panel(Some("bob"), personas, Tenacity::Standard);
        let mut persist = ok_persist();
        s.begin_command("w clone");
        assert_eq!(s.run_command(&mut persist), None);
        assert_eq!(s.saved.as_deref(), Some("clone"));
        // The content passed to persist reproduces the projection.
        let content = s.persona_content("clone");
        let rp = newt_core::RoleProfile::parse(&content).unwrap();
        assert_eq!(rp.cognition, Some(Cognition::Contemplating));
        assert_eq!(rp.tenacity, Some(Tenacity::Relentless));
    }

    #[test]
    fn persona_row_shows_active_and_maps_none_to_clear() {
        let _g = GlobalSettingsGuard::acquire();
        let mut s = panel(Some("bob"), two_personas(), Tenacity::Standard);
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
        assert!(s.persona_label().contains("clears active"));
        // Cycle to obsessive → Switch + "(pending)".
        s.cycle(1); // bob
        s.cycle(1); // obsessive
        assert_eq!(
            s.persona_action(),
            PersonaAction::Switch("obsessive".to_string())
        );
        assert!(s.persona_label().contains("(pending)"));
    }

    #[test]
    fn wq_success_saves_then_signals_apply() {
        // review-3 §1: :wq whose save lands → close-with-apply, and `saved` records.
        let _g = GlobalSettingsGuard::acquire();
        let mut s = panel(None, two_personas(), Tenacity::Standard);
        let mut persist = ok_persist();
        s.begin_command("wq alice");
        assert_eq!(
            s.run_command(&mut persist),
            Some(true),
            ":wq closes with apply"
        );
        assert_eq!(s.saved.as_deref(), Some("alice"));
    }

    #[test]
    fn wq_failed_save_does_not_apply_and_stays_open() {
        // review-3 §1: a failed persist must NOT close/apply; the panel stays open
        // with a visible error and records nothing as saved.
        let _g = GlobalSettingsGuard::acquire();
        let mut s = panel(None, two_personas(), Tenacity::Standard);
        // change a dial so we can prove it was NOT applied
        s.down(); // cognition
        s.cycle(1); // auto → off (dirty)
        assert!(s.cognition.is_dirty());
        let mut persist =
            |_n: &str, _c: &str, _o: bool| SaveResult::Failed("disk full".to_string());
        s.begin_command("wq alice");
        assert_eq!(
            s.run_command(&mut persist),
            None,
            "failed :wq does NOT close (so the caller never applies)"
        );
        assert!(s.saved.is_none(), "nothing recorded as saved");
        assert!(
            s.status.as_deref().unwrap().contains("disk full"),
            "the failure is visible"
        );
        // The dirty dial is untouched in the working copy; apply() was never called.
        assert!(s.cognition.is_dirty(), "edits intact for a retry");
    }

    #[test]
    fn wq_exists_without_bang_is_refused_visibly() {
        let _g = GlobalSettingsGuard::acquire();
        let mut s = panel(None, two_personas(), Tenacity::Standard);
        let mut persist = |name: &str, _c: &str, overwrite: bool| {
            if overwrite {
                SaveResult::Saved {
                    name: name.to_string(),
                }
            } else {
                SaveResult::Exists {
                    name: name.to_string(),
                }
            }
        };
        s.begin_command("wq bob");
        assert_eq!(s.run_command(&mut persist), None, "exists → stay open");
        assert!(s.status.as_deref().unwrap().contains("exists"));
        assert!(s.saved.is_none());
        // With the bang it overwrites and applies.
        s.begin_command("wq! bob");
        assert_eq!(s.run_command(&mut persist), Some(true));
        assert_eq!(s.saved.as_deref(), Some("bob"));
    }

    #[test]
    fn ex_commands_validate_visibly() {
        let _g = GlobalSettingsGuard::acquire();
        let mut s = panel(None, two_personas(), Tenacity::Standard);
        let mut persist = ok_persist();
        // :wq with no name → refuse, stay open, visible status.
        s.begin_command("wq");
        assert_eq!(
            s.run_command(&mut persist),
            None,
            ":wq without a name does not close"
        );
        assert!(s.status.as_deref().unwrap().contains("needs a name"));
        assert!(s.saved.is_none());
        // Unknown command → visible error.
        s.begin_command("banana");
        assert_eq!(s.run_command(&mut persist), None);
        assert!(s.status.as_deref().unwrap().contains("unknown command"));
        // :q → cancel + close.
        s.begin_command("q");
        assert_eq!(s.run_command(&mut persist), Some(false));
    }

    #[test]
    fn w_then_cancel_is_saved_not_applied() {
        // Driving `run` needs a TTY, so assert the OUTCOME mapping directly: a save
        // happened (`saved` set) but the loop was cancelled (applied = false) →
        // PanelOutcome::Saved, never a posture summary from the working copy.
        let _g = GlobalSettingsGuard::acquire();
        let mut s = panel(None, two_personas(), Tenacity::Standard);
        let mut persist = ok_persist();
        s.begin_command("w keep");
        s.run_command(&mut persist);
        assert_eq!(s.saved.as_deref(), Some("keep"));
        // Simulate the cancel branch of `run` (applied = false):
        let outcome = match s.saved.clone() {
            Some(name) => PanelOutcome::Saved { name },
            None => PanelOutcome::Cancelled,
        };
        assert_eq!(
            outcome,
            PanelOutcome::Saved {
                name: "keep".to_string()
            }
        );
    }
}
