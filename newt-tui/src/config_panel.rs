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
//!   The model row (#1666) spins through the active backend's served models;
//!   its pick is applied by the CALLER through the `/model` path.
//! - `Enter` — apply the changed dials + act on the persona choice, close.
//!   With nothing changed (no dirty dial, persona kept), Enter closes silently
//!   — a no-op visit applies nothing and reports nothing (#1665).
//! - `Esc` / `q` — cancel: discard changes, close (never applies).
//! - `Ctrl-S` or `:w <name>` — save the posture as persona `<name>` (`:w!` to
//!   overwrite). `:wq <name>` — save + apply + close. `:q` — cancel + close.
//!
//! [`PanelState`] is pure and unit-tested; the raw-mode loop ([`run`]) is
//! TUI-drive tested.

use std::io;

use crate::panel::Key;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use newt_core::cognition::{cli_cognition, CognitionOverride};
use newt_core::role_profile::Cognition;
use newt_core::tenacity::{cli_tenacity, Tenacity};

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
    Model,
    Cognition,
    Tenacity,
}

const ROWS: [Row; 4] = [Row::Persona, Row::Model, Row::Cognition, Row::Tenacity];

/// One entry in the model spinner (#1666): a model the active backend serves,
/// plus its cached conformance tag (the same symbol `/models` prints; empty
/// when untested). Built by the caller — the panel stays network-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelChoice {
    pub(crate) name: String,
    pub(crate) tag: String,
}

/// Everything the CALLER resolves before the panel opens — the panel's input
/// record (one struct rather than a positional list that outgrew clippy's
/// argument limit when #1666 added the model spinner): the selectable
/// personas + the active one, the operator baseline backend, the
/// config/family tenacity base, and the served-model list + active model.
pub(crate) struct PanelSeed {
    /// The route the operator actually typed — `/psyche` or `/psyche edit`,
    /// both of which open this panel (see `chat.rs`'s `wants_psyche_panel`).
    ///
    /// Carried rather than assumed: it is what the settings receipt records as
    /// `via`, and the whole value of that column is being the half of the
    /// event a reader cannot reconstruct from the resulting state. Hardcoding
    /// `/psyche` here would journal a route the operator never typed for half
    /// the ways in — a false record, which is worse than none.
    pub via: &'static str,
    pub personas: Vec<PersonaChoice>,
    pub current_persona: Option<String>,
    pub backend: Option<String>,
    pub base_tenacity: Tenacity,
    pub models: Option<Vec<ModelChoice>>,
    pub current_model: String,
}

/// A value the operator may have changed: `Inherit` (untouched — do NOT write) or
/// `Set` (dirty — write on apply). Shared with the backend panel (#1667), which
/// follows the same dirty-tracking grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dial<T> {
    Inherit(T),
    Set(T),
}

impl<T: Copy> Dial<T> {
    pub(crate) fn value(self) -> T {
        match self {
            Self::Inherit(v) | Self::Set(v) => v,
        }
    }
    pub(crate) fn is_dirty(self) -> bool {
        matches!(self, Self::Set(_))
    }
    pub(crate) fn set(&mut self, v: T) {
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
    /// Enter / `:wq`-with-no-save: dials were applied; act on `persona`, and
    /// route `model` (the spinner pick, `None` = untouched) through the same
    /// path `/model <name>` takes — the session owns that state (#1666).
    Applied {
        persona: PersonaAction,
        model: Option<String>,
    },
    /// `:w` then cancel: the file was persisted, but dials were NOT applied.
    Saved { name: String },
    /// `:wq`: the file was persisted AND dials were applied; act on `persona`
    /// and `model` exactly as in [`PanelOutcome::Applied`].
    SavedAndApplied {
        name: String,
        persona: PersonaAction,
        model: Option<String>,
    },
}

/// The panel's working state. Pure: no terminal, no I/O; fully unit-testable.
/// Persistence is injected into [`PanelState::run_command`] as a closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PanelState {
    /// The route that opened this panel, recorded as the receipt's `via`.
    via: &'static str,
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
    /// The active backend's served models (#1666); `None` = the backend could
    /// not be listed when the panel opened — the row renders but won't dial.
    model_opts: Option<Vec<ModelChoice>>,
    /// Spinner position into `model_opts` (meaningless when `model_opts` is
    /// `None`). Dirty = the operator touched the spinner.
    model: Dial<usize>,
    /// The model the session resolved when the panel opened — the "(active)"
    /// marker + the value a no-op visit leaves untouched.
    current_model: String,
    /// Tenacity with no CLI override and no persona layer (config per-family /
    /// default / `Standard`) — the value a persona that declares none inherits.
    base_tenacity: Tenacity,
    /// The crew launch gate at open (`NEWT_TEAM`) — the base a persona's `crew:`
    /// declaration projects over.
    base_crew: bool,
    /// The operator BASELINE backend (what a no-backend persona reverts to on
    /// apply) — the projection fallback + save default.
    backend: Option<String>,
    mode: Mode,
    /// Transient status / error line (visible feedback for saves + bad commands).
    status: Option<String>,
    /// The name of the most recent SUCCESSFUL save this session (`None` until a
    /// `:w`/`:wq` write actually lands). Distinct from a proposed edit.
    saved: Option<String>,
}

impl PanelState {
    /// Seed from the live overrides via [`PanelSeed`] — the caller resolves
    /// everything; the panel stays network- and config-free.
    pub(crate) fn new(seed: PanelSeed) -> Self {
        let PanelSeed {
            via,
            mut personas,
            current_persona,
            backend,
            base_tenacity,
            models,
            current_model,
        } = seed;
        let current_model = current_model.as_str();
        // Same guarantee as the persona ghost below (#1666): the ACTIVE model is
        // always selectable, even when the backend's list omits it (stale list,
        // model just unloaded) — otherwise opening the panel would silently
        // reposition the spinner and Enter could apply an unchosen model. An
        // empty served list degrades to unreachable (nothing to dial).
        let model_opts = models
            .map(|mut list| {
                if !current_model.is_empty() && !list.iter().any(|m| m.name == current_model) {
                    list.push(ModelChoice {
                        name: current_model.to_string(),
                        tag: "(not served)".to_string(),
                    });
                }
                list
            })
            .filter(|list| !list.is_empty());
        let model_idx = model_opts
            .as_ref()
            .and_then(|list| list.iter().position(|m| m.name == current_model))
            .unwrap_or(0);
        // Guarantee the active persona is selectable. If it isn't in the list
        // (e.g. its file failed to load), append it so it renders "(active)" and
        // Enter maps to Keep — never a silent Clear (review-3 follow-up). Its
        // declarations are unknown, so it projects the base (Keep changes nothing).
        if let Some(cur) = &current_persona {
            if !personas.iter().any(|p| &p.name == cur) {
                personas.push(PersonaChoice {
                    name: cur.clone(),
                    cognition: None,
                    tenacity: None,
                    backend: None,
                    crew: None,
                });
            }
        }
        let mut persona_opts = Vec::with_capacity(personas.len() + 1);
        persona_opts.push(NONE.to_string());
        persona_opts.extend(personas.iter().map(|p| p.name.clone()));
        let persona_idx = current_persona
            .as_ref()
            .and_then(|c| persona_opts.iter().position(|n| n == c))
            .unwrap_or(0);
        Self {
            via,
            sel: 0,
            persona_opts,
            personas,
            persona_idx,
            current_persona,
            cognition: Dial::Inherit(cli_cognition()),
            tenacity: Dial::Inherit(cli_tenacity()),
            model_opts,
            model: Dial::Inherit(model_idx),
            current_model: current_model.to_string(),
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
            Row::Model => {
                // Unreachable backend → nothing to dial; the row says why.
                if let Some(opts) = &self.model_opts {
                    self.model
                        .set(clamp_step(self.model.value(), dir, opts.len()));
                }
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

    /// Apply the dials the operator moved — **through the one mutation path,
    /// which is what writes the receipt** (#1965).
    ///
    /// `auto`/`inherit` CLEARS the override so the value returns to
    /// persona/config resolution. Called by [`run`] only after an explicit
    /// apply (Enter / a `:wq` whose save landed).
    ///
    /// This used to call `set_cli_cognition` / `set_cli_tenacity` /
    /// `clear_cli_tenacity` directly. It was the last live bypass of
    /// `settings_form::apply_and_record`, and the worst-placed one: a panel
    /// whose entire job is changing dials, leaving nothing durable behind. The
    /// slash verbs it duplicates (`/psyche cognition`, `/psyche tenacity`) have
    /// been journalling since #1981, so the same change made two ways produced
    /// a record only one way — the exact shape of the audit finding that an
    /// unlimited round cap left no trace.
    ///
    /// `via` is the route the panel was OPENED by — `/psyche` or `/psyche
    /// edit`, both of which land here — carried in on [`PanelSeed`] rather
    /// than hardcoded. The route is the half of the event a reader cannot
    /// reconstruct from the resulting state, so a receipt naming a command the
    /// operator never typed is a false record rather than a rounding error.
    ///
    /// Only DIRTY dials are touched, unchanged: #1668's posture marks say the
    /// operator moved THIS axis, so an untouched row of the panel must pin
    /// nothing — and a receipt for a dial nobody touched would be a false
    /// entry in the journal. (The model spinner's pick is marked on the
    /// `/model` path the caller routes it through, after that path's
    /// served-validation gate.)
    pub(crate) fn apply(&self) {
        if self.cognition.is_dirty() {
            let _ = crate::settings_form::apply_cognition(self.cognition.value(), self.via);
        }
        if self.tenacity.is_dirty() {
            let _ = crate::settings_form::apply_tenacity(self.tenacity.value(), self.via);
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

    /// True when applying would change nothing: no dial is dirty and the persona
    /// choice is Keep. [`run`] downgrades an Enter on a no-op panel to a silent
    /// close (#1665) — bare `/psyche` opens the panel, so browsing must never
    /// mutate or report. A dial cycled away and back is dirty (a deliberate
    /// same-value re-apply), so it still counts as a change.
    pub(crate) fn is_noop(&self) -> bool {
        !self.cognition.is_dirty()
            && !self.tenacity.is_dirty()
            && !self.model.is_dirty()
            && self.persona_action() == PersonaAction::Keep
    }

    /// The model spinner's pick, `None` when untouched (#1666). A touched
    /// spinner is a pick even at the original position — same deliberate
    /// re-apply semantics as the dials — and the CALLER routes it through the
    /// `/model` path (validation gate, persistence rules, warmup); the panel
    /// itself never mutates model state.
    pub(crate) fn chosen_model(&self) -> Option<String> {
        if !self.model.is_dirty() {
            return None;
        }
        self.model_opts
            .as_ref()
            .and_then(|opts| opts.get(self.model.value()))
            .map(|m| m.name.clone())
    }

    fn model_label(&self) -> String {
        let Some(opts) = &self.model_opts else {
            return "(backend unreachable — /models when it's back)".to_string();
        };
        let m = &opts[self.model.value()];
        let tag = if m.tag.is_empty() {
            String::new()
        } else {
            format!(" {}", m.tag)
        };
        if m.name == self.current_model {
            format!("{}{tag} (active)", m.name)
        } else {
            format!("{}{tag} (pending)", m.name)
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
    /// else the operator baseline (what a no-backend persona reverts to on apply).
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
                label: "model",
                value: self.model_label(),
                provenance: if self.model_opts.is_some() {
                    "active backend".to_string()
                } else {
                    String::new()
                },
                selected: ROWS[self.sel] == Row::Model,
                editable: self.model_opts.is_some(),
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
            // No declared backend → the selection reverts to the operator baseline
            // on apply (NOT the outgoing persona's backend).
            _ => "base".to_string(),
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

/// A rendered row: label, value, provenance, and display flags. Shared with the
/// backend panel (#1667) so every panel renders rows the same way.
pub(crate) struct RowView {
    pub(crate) label: &'static str,
    pub(crate) value: String,
    pub(crate) provenance: String,
    pub(crate) selected: bool,
    pub(crate) editable: bool,
}

fn sanitize_name(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

/// Clamped (non-wrapping) spinner step — shared by every panel dial.
///
/// **Total, including on an empty list.** The obvious form,
/// `(i + dir).clamp(0, len - 1)`, PANICS at `len == 0`: that is `clamp(0, -1)`,
/// and `clamp` requires `min <= max`. No caller reaches it today, and each one
/// is safe for a different reason — the backend chooser always carries two
/// unremovable kind fallbacks, the psyche panel filters an empty model list to
/// `None`, the settings panel guards both dial paths, and the ladders are
/// consts. Four accidents of construction, none of them stated, all of them
/// one refactor from being wrong.
///
/// So the helper is made total rather than each caller made careful: an empty
/// list has no positions, and the only honest answer for "where is the cursor"
/// is 0. That is CLAUDE.md's rule — prefer making a bug unrepresentable over
/// fixing each site — applied to the one place that can enforce it.
pub(crate) fn clamp_step(i: usize, dir: i32, len: usize) -> usize {
    let Some(last) = len.checked_sub(1) else {
        return 0;
    };
    let n = i32::try_from(last).unwrap_or(i32::MAX);
    (i32::try_from(i).unwrap_or(i32::MAX) + dir).clamp(0, n) as usize
}

/// Bordered block (2) + six rows + a hint/command/status row.
const PANEL_HEIGHT: u16 = 10;

fn draw(f: &mut ratatui::Frame, state: &PanelState) {
    let bottom = if let Mode::Command(buf) = &state.mode {
        command_line(buf)
    } else if let Some(status) = &state.status {
        status_line(status)
    } else {
        hint_line(
            "↑↓ select · ←→ change (auto=inherit) · Enter apply · Esc cancel · Ctrl-S/:w <name> save",
        )
    };
    render_panel(
        f,
        " psyche — operator dials ",
        &state.view_rows(),
        bottom,
        11,
        26,
    );
}

/// The shared panel chrome (#1667): bordered block + title, one line per
/// [`RowView`] (selected rows get the `❯` marker and `‹ ›` dial chrome, dim
/// provenance column), and the caller's bottom line. `label_w`/`val_w` are the
/// per-panel column widths.
pub(crate) fn render_panel(
    f: &mut ratatui::Frame,
    title: &str,
    rows: &[RowView],
    bottom: Line,
    label_w: usize,
    val_w: usize,
) {
    // The edge comes from `modal::frame`, which every dialog now shares. The
    // panel used to own the only border in the crate; making it the SHARED one
    // is how the pagers and the permission prompt got an edge at all.
    let inner = crate::modal::frame(
        f,
        f.area(),
        &crate::modal::Chrome {
            title: title.trim(),
            ..crate::modal::Chrome::default()
        },
    );

    let mut lines: Vec<Line> = Vec::new();
    for row in rows {
        let marker = if row.selected { "❯ " } else { "  " };
        let name = format!("{marker}{:<label_w$}", row.label);
        let val = if row.selected && row.editable {
            format!("‹ {} ›", row.value)
        } else {
            row.value.clone()
        };
        let (name_style, val_style) = row_styles(row.selected, row.editable);
        let mut spans = vec![
            Span::styled(name, name_style),
            Span::styled(format!("{val:<val_w$}"), val_style),
        ];
        if !row.provenance.is_empty() {
            spans.push(Span::styled(
                row.provenance.clone(),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        lines.push(Line::from(spans));
    }
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

/// The `:…▏` ex-command bottom line — shared panel chrome (#1667).
pub(crate) fn command_line(buf: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!(":{buf}▏"),
        Style::default()
            .fg(crate::theme::color(crate::theme::Role::Ok))
            .add_modifier(Modifier::BOLD),
    ))
}

/// The status bottom line — shared panel chrome (#1667).
pub(crate) fn status_line(status: &str) -> Line<'static> {
    Line::from(Span::styled(
        status.to_string(),
        Style::default()
            .fg(crate::theme::color(crate::theme::Role::Identity))
            .add_modifier(Modifier::BOLD),
    ))
}

/// The dim key-hint bottom line — shared panel chrome (#1667).
pub(crate) fn hint_line(hint: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        hint,
        Style::default().add_modifier(Modifier::DIM),
    ))
}

/// Row styling shared across panels (#1667): selected = bold label/value,
/// read-only = dim.
///
/// The four colours this file hardcoded — Cyan, Yellow, Green, Magenta — are
/// where `SelectedLabel`, `SelectedValue`, `Ok` and `Identity` came from: the
/// colour census that minted those roles read this panel and then never wired
/// them back, so they sat in `theme.rs` with zero consumers while the literals
/// they were named after stayed put. Same bytes, now themeable.
pub(crate) fn row_styles(selected: bool, editable: bool) -> (Style, Style) {
    if selected {
        (
            Style::default()
                .fg(crate::theme::color(crate::theme::Role::SelectedLabel))
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(crate::theme::color(crate::theme::Role::SelectedValue))
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
/// An Enter with nothing changed ([`PanelState::is_noop`]) downgrades to
/// [`PanelOutcome::Cancelled`] — bare `/psyche` opens this panel (#1665), so a
/// browse-and-leave visit must be indistinguishable from never opening it.
pub(crate) fn run(
    seed: PanelSeed,
    persist: impl FnMut(&str, &str, bool) -> SaveResult,
) -> io::Result<PanelOutcome> {
    let mut screen = PsycheScreen {
        state: PanelState::new(seed),
        persist,
    };
    // Scoped inside `panel::drive`, whose raw-mode guard restores exactly
    // where the old bare `disable_raw_mode()` statement did (#1889).
    let applied = crate::panel::drive(&mut screen, PANEL_HEIGHT, None)?;

    // Commit order (review-3 §1): the persona file was already persisted inside
    // the loop (via `persist`). Now apply the dials — but ONLY on an explicit
    // apply — then hand the persona action to the caller, which applies it, reroutes
    // the backend, recomputes, and reports from fresh runtime state.
    Ok(close_outcome(applied, &screen.state))
}

/// The psyche panel as a [`crate::panel::Screen`]: its state, its writer, and
/// its key table. Everything else the loop used to do is the driver's.
struct PsycheScreen<P: FnMut(&str, &str, bool) -> SaveResult> {
    state: PanelState,
    persist: P,
}

impl<P: FnMut(&str, &str, bool) -> SaveResult> crate::panel::Screen for PsycheScreen<P> {
    fn draw(&self, frame: &mut ratatui::Frame) {
        draw(frame, &self.state);
    }

    fn key(&mut self, key: Key) -> crate::panel::Flow {
        use crate::panel::Flow;
        if self.state.in_command() {
            match key {
                // `Char` is now a PLAIN character by construction, so a second
                // Ctrl-S while the line is open no longer types a literal `s`
                // into the line Ctrl-S opened.
                Key::Char(c) => self.state.command_char(c),
                Key::Backspace => self.state.command_backspace(),
                Key::Esc => self.state.cancel_command(),
                Key::Enter => {
                    if let Some(apply) = self.state.run_command(&mut self.persist) {
                        return Flow::Close(apply);
                    }
                }
                _ => {}
            }
            return Flow::Stay;
        }
        match key {
            Key::Up => self.state.up(),
            Key::Down => self.state.down(),
            Key::Left => self.state.cycle(-1),
            Key::Right => self.state.cycle(1),
            // An explicit control binding, said in the pattern rather than in
            // a guard beside it.
            Key::Ctrl('s') => self.state.begin_command("w "),
            Key::Char(':') => self.state.begin_command(""),
            Key::Enter => return Flow::Close(true),
            Key::Esc | Key::Char('q') => return Flow::Close(false),
            _ => {}
        }
        Flow::Stay
    }
}

/// The panel's exit contract, factored out of the raw-mode loop so it is
/// unit-testable without a TTY (adversarial-review finding on #1665): given
/// whether the operator explicitly applied (Enter / a `:wq` whose save landed)
/// and the final working state, produce the [`PanelOutcome`] — running
/// [`PanelState::apply`] as a side effect ONLY on a real (non-noop) apply.
/// The noop downgrade lives HERE: Enter with nothing changed maps to
/// `Cancelled` (or `Saved` after a lone `:w`, so the save is still reported).
fn close_outcome(applied: bool, state: &PanelState) -> PanelOutcome {
    if applied && !state.is_noop() {
        state.apply();
        let persona = state.persona_action();
        let model = state.chosen_model();
        match state.saved.clone() {
            Some(name) => PanelOutcome::SavedAndApplied {
                name,
                persona,
                model,
            },
            None => PanelOutcome::Applied { persona, model },
        }
    } else {
        match state.saved.clone() {
            Some(name) => PanelOutcome::Saved { name },
            None => PanelOutcome::Cancelled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The setters the panel no longer calls: tests still use them to ARRANGE
    // runtime state, which is not the same as writing a setting.
    use newt_core::cognition::set_cli_cognition;
    use newt_core::tenacity::{clear_cli_tenacity, set_cli_tenacity};
    use newt_core::test_guard::GlobalSettingsGuard;

    /// The panel is newt's only bordered box, and it shipped with no
    /// `border_style` — so the frame around a dialog that had TAKEN THE
    /// KEYBOARD drew in whatever the terminal last set, i.e. exactly like
    /// ordinary output. This pins that the frame, the title, and the selected
    /// row all read their colour from the theme rather than a literal.
    ///
    /// It also guards the failure that motivated the change: `SelectedLabel`,
    /// `SelectedValue`, `Ok` and `Identity` were minted from this file's
    /// hardcoded Cyan/Yellow/Green/Magenta and then never wired back, so they
    /// sat in `theme.rs` with zero consumers. A role with no consumer is a
    /// theme that silently does nothing.
    #[test]
    fn panel_chrome_reads_its_colours_from_the_theme() {
        use crate::theme::Role;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let rows = vec![
            RowView {
                label: "persona",
                value: "sol".to_string(),
                provenance: String::new(),
                selected: true,
                editable: true,
            },
            RowView {
                label: "backend",
                value: "dgx".to_string(),
                provenance: String::new(),
                selected: false,
                editable: false,
            },
        ];

        let mut term = Terminal::new(TestBackend::new(48, 6)).unwrap();
        term.draw(|f| render_panel(f, " settings ", &rows, hint_line("q quit"), 10, 12))
            .unwrap();
        let buf = term.backend().buffer().clone();

        // The frame. Top-left corner is border, and it is NOT the terminal
        // default — which is the whole bug.
        let corner = buf.cell((0, 0)).unwrap();
        assert_eq!(
            corner.symbol(),
            "╭",
            "the box draws, now through the SHARED chrome — rounded, like every \
             other modal, which is the point of the shared edge"
        );
        assert_eq!(
            corner.fg,
            crate::theme::color(Role::ModalBorder),
            "the border is themed, not left at the terminal default"
        );
        assert_ne!(
            corner.fg,
            ratatui::style::Color::Reset,
            "an unstyled border is the defect this test exists for"
        );

        // The title, somewhere along the top edge.
        let titled = (1..47)
            .map(|x| buf.cell((x, 0)).unwrap())
            .find(|c| c.symbol() == "s")
            .expect("the title renders on the top edge");
        assert_eq!(titled.fg, crate::theme::color(Role::ModalTitle));

        // The selected row's label and value take the two roles this file's
        // literals originally named.
        let row: Vec<_> = (0..48).map(|x| buf.cell((x, 1)).unwrap()).collect();
        let label = row
            .iter()
            .find(|c| c.symbol() == "p")
            .expect("the selected label renders");
        assert_eq!(label.fg, crate::theme::color(Role::SelectedLabel));
        let value = row
            .iter()
            .find(|c| c.symbol() == "‹")
            .expect("the selected row carries dial chrome");
        assert_eq!(value.fg, crate::theme::color(Role::SelectedValue));
    }

    /// The two shared bottom lines take the other two once-orphaned roles.
    #[test]
    fn bottom_lines_read_their_colours_from_the_theme() {
        use crate::theme::Role;

        let ex = command_line("w");
        assert_eq!(ex.spans[0].style.fg, Some(crate::theme::color(Role::Ok)));
        let status = status_line("saved");
        assert_eq!(
            status.spans[0].style.fg,
            Some(crate::theme::color(Role::Identity))
        );
    }

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

    fn model_choices(names: &[&str]) -> Vec<ModelChoice> {
        names
            .iter()
            .map(|n| ModelChoice {
                name: n.to_string(),
                tag: String::new(),
            })
            .collect()
    }

    fn seed(
        current: Option<&str>,
        personas: Vec<PersonaChoice>,
        base_ten: Tenacity,
        models: Option<Vec<ModelChoice>>,
        current_model: &str,
    ) -> PanelSeed {
        PanelSeed {
            via: "/psyche",
            personas,
            current_persona: current.map(str::to_string),
            backend: Some("sol".to_string()),
            base_tenacity: base_ten,
            models,
            current_model: current_model.to_string(),
        }
    }

    fn panel(
        current: Option<&str>,
        personas: Vec<PersonaChoice>,
        base_ten: Tenacity,
    ) -> PanelState {
        PanelState::new(seed(
            current,
            personas,
            base_ten,
            Some(model_choices(&["m1", "m2", "m3"])),
            "m2",
        ))
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

    /// #1668: a panel apply marks a posture ACTION for exactly its DIRTY dials
    /// — so browsing the panel (bare `/psyche` opens it) can never pin, and a
    /// one-dial edit cannot drag the other dial's ambient value into the pin.
    /// **The dial step is total**, including on the empty list no caller
    /// reaches today.
    ///
    /// The naive `clamp(0, len - 1)` panics at `len == 0` — `clamp` requires
    /// `min <= max`. Every current caller is safe for a DIFFERENT reason (two
    /// unremovable kind fallbacks; an empty model list filtered to `None`; two
    /// guarded dial paths; const ladders), which is four unstated accidents
    /// rather than one enforced rule. This is the rule.
    #[test]
    fn the_dial_step_is_total_and_clamps_at_both_ends() {
        // The case that used to panic.
        assert_eq!(clamp_step(0, 1, 0), 0, "an empty list has no positions");
        assert_eq!(clamp_step(0, -1, 0), 0);
        assert_eq!(clamp_step(7, 1, 0), 0, "even from a stale index");

        // A one-item list is a fixed point in both directions.
        assert_eq!(clamp_step(0, 1, 1), 0);
        assert_eq!(clamp_step(0, -1, 1), 0);

        // And the ordinary case still clamps rather than wrapping.
        assert_eq!(clamp_step(0, -1, 3), 0, "the floor holds");
        assert_eq!(clamp_step(2, 1, 3), 2, "the ceiling holds");
        assert_eq!(clamp_step(1, 1, 3), 2);
        assert_eq!(clamp_step(1, -1, 3), 0);

        // A stale index above the end walks back into range rather than
        // reaching further out — the shape a shrinking list produces.
        assert_eq!(clamp_step(9, -1, 3), 2);
        assert_eq!(clamp_step(9, 1, 3), 2);
    }

    /// **The panel cannot reach a dial except through the recorder** (#1965).
    ///
    /// `PanelState::apply` used to call `set_cli_cognition` /
    /// `set_cli_tenacity` / `clear_cli_tenacity` directly — the last live
    /// bypass of `settings_form`'s one mutation path, and the worst-placed
    /// one: a panel whose whole job is moving dials, leaving nothing durable
    /// behind, while the slash verbs it duplicates had been journalling since
    /// #1981.
    ///
    /// Counted over PRODUCTION source because the property is "no other path
    /// exists", and absence is not observable by calling something. The
    /// behavioural half — that the dials still move, and that only the dirty
    /// ones pin a preference — is the test below and
    /// `close_outcome_downgrades_a_noop_enter_and_applies_a_real_one`.
    #[test]
    fn the_panel_writes_dials_only_through_the_recorded_path() {
        let source = crate::production_source(include_str!("config_panel.rs"));
        for setter in [
            "set_cli_cognition(",
            "set_cli_tenacity(",
            "clear_cli_tenacity(",
            "mark_cognition_choice(",
            "mark_tenacity_choice(",
        ] {
            assert_eq!(
                source.matches(setter).count(),
                0,
                "`{setter}` writes a setting with no receipt; go through \
                 settings_form::apply_cognition / apply_tenacity"
            );
        }
        assert!(
            source.contains("settings_form::apply_cognition")
                && source.contains("settings_form::apply_tenacity"),
            "the panel must apply its dials through the recording path"
        );
    }

    #[test]
    fn apply_marks_a_preference_action_only_for_the_dirty_dials() {
        use newt_core::runtime::drain_preference_actions;
        let _g = GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Unset);
        clear_cli_tenacity();
        let _ = drain_preference_actions();

        // A browse-and-apply with nothing touched marks nothing.
        let s = panel(None, two_personas(), Tenacity::Standard);
        s.apply();
        assert!(
            drain_preference_actions().is_empty(),
            "an untouched panel must not pin"
        );

        // Touch ONLY cognition → only the cognition axis is marked.
        let mut s = panel(None, two_personas(), Tenacity::Standard);
        s.down(); // → model
        s.down(); // → cognition
        s.cycle(1);
        assert!(s.cognition.is_dirty() && !s.tenacity.is_dirty());
        s.apply();
        let a = drain_preference_actions();
        assert_eq!(a.cognition, Some(s.cognition.value()));
        assert_eq!(a.tenacity, None, "an untouched dial must not be pinned");
        assert_eq!((a.backend, a.model), (None, None), "panel owns dials only");

        // Touch ONLY tenacity, to its `auto` position → the axis is UNPINNED.
        let mut s = panel(None, two_personas(), Tenacity::Standard);
        s.down();
        s.down();
        s.down(); // → tenacity
        for _ in 0..tenacity_ladder().len() {
            s.cycle(-1);
        }
        assert_eq!(s.tenacity.value(), None, "reached auto");
        s.apply();
        let a = drain_preference_actions();
        assert_eq!(a.tenacity, Some(None), "auto unpins the axis");
        assert_eq!(a.cognition, None);
    }

    #[test]
    fn auto_position_clears_the_tenacity_override() {
        let _g = GlobalSettingsGuard::acquire();
        set_cli_tenacity(Tenacity::Relentless);
        let mut s = panel(None, two_personas(), Tenacity::Standard);
        s.down(); // → model
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
    fn active_persona_absent_from_the_list_is_kept_not_silently_cleared() {
        // review-3 follow-up: if the active persona isn't in the projected list
        // (e.g. its file failed to load), it must still show "(active)" and Enter
        // must map to Keep — never a silent Clear at index 0.
        let _g = GlobalSettingsGuard::acquire();
        let s = panel(Some("ghost"), two_personas(), Tenacity::Standard);
        assert!(s.persona_label().contains("ghost"));
        assert!(s.persona_label().contains("(active)"));
        assert_eq!(s.persona_action(), PersonaAction::Keep);
    }

    #[test]
    fn a_noop_visit_is_detected_so_enter_closes_silently() {
        // #1665: bare /psyche opens the panel, so browsing-then-Enter must be a
        // no-op — nothing applied, nothing reported. Moving the selection is
        // not a change; touching a dial (even back to its original value) is.
        let _g = GlobalSettingsGuard::acquire();
        let mut s = panel(Some("bob"), two_personas(), Tenacity::Standard);
        assert!(s.is_noop(), "fresh panel: nothing to apply");
        s.down();
        s.up();
        assert!(s.is_noop(), "row selection alone is not a change");
        // Touch the cognition dial: dirty even after cycling back — a deliberate
        // same-value re-apply is still an operator action.
        s.down(); // persona → model
        s.down(); // model → cognition
        s.cycle(1);
        assert!(!s.is_noop(), "a touched dial is a change");
        s.cycle(-1);
        assert!(!s.is_noop(), "cycled back is still dirty (re-apply)");
        // A persona switch is a change too.
        let mut p = panel(Some("bob"), two_personas(), Tenacity::Standard);
        while p.persona_idx != 0 {
            p.cycle(-1);
        }
        assert!(!p.is_noop(), "persona Clear is a change");
        // A touched model spinner is a change too (#1666).
        let mut m = panel(Some("bob"), two_personas(), Tenacity::Standard);
        m.down(); // persona → model
        m.cycle(1);
        assert!(!m.is_noop(), "a touched model spinner is a change");
    }

    #[test]
    fn model_spinner_dials_served_models_and_reports_only_a_touched_pick() {
        // #1666: the spinner opens ON the active model, ←/→ moves through the
        // served list, and chosen_model() reports None until touched.
        let _g = GlobalSettingsGuard::acquire();
        let mut s = panel(None, two_personas(), Tenacity::Standard);
        assert_eq!(s.chosen_model(), None, "untouched spinner picks nothing");
        assert!(
            s.model_label().contains("m2") && s.model_label().contains("(active)"),
            "opens on the active model: {}",
            s.model_label()
        );
        s.down(); // persona → model
        s.cycle(1); // m2 → m3
        assert_eq!(s.chosen_model().as_deref(), Some("m3"));
        // The pick rides the outcome through the REAL exit path.
        assert_eq!(
            close_outcome(true, &s),
            PanelOutcome::Applied {
                persona: PersonaAction::Keep,
                model: Some("m3".to_string()),
            }
        );
        assert!(
            s.model_label().contains("(pending)"),
            "a non-active position renders pending: {}",
            s.model_label()
        );
        // Touched-back-to-original is still a deliberate re-apply pick.
        s.cycle(-1);
        assert_eq!(s.chosen_model().as_deref(), Some("m2"));
        // Left edge clamps (no wrap): m2 → m1 → m1.
        s.cycle(-1);
        s.cycle(-1);
        assert_eq!(s.chosen_model().as_deref(), Some("m1"));
    }

    #[test]
    fn an_active_model_missing_from_the_served_list_is_appended_not_lost() {
        // #1666: same guarantee as the persona ghost — the panel must open ON
        // the active model even when the backend's list omits it, or Enter
        // could apply a silently repositioned spinner.
        let _g = GlobalSettingsGuard::acquire();
        let s = PanelState::new(seed(
            None,
            two_personas(),
            Tenacity::Standard,
            Some(model_choices(&["m1", "m2"])),
            "ghost-model",
        ));
        assert!(
            s.model_label().contains("ghost-model")
                && s.model_label().contains("(not served)")
                && s.model_label().contains("(active)"),
            "{}",
            s.model_label()
        );
        assert_eq!(s.chosen_model(), None, "still untouched");
    }

    #[test]
    fn an_unreachable_backend_disables_the_model_dial() {
        // #1666: no served list → the row says why, won't dial, never picks.
        let _g = GlobalSettingsGuard::acquire();
        let mut s = PanelState::new(seed(None, two_personas(), Tenacity::Standard, None, "m2"));
        assert!(
            s.model_label().contains("unreachable"),
            "{}",
            s.model_label()
        );
        s.down(); // persona → model
        s.cycle(1);
        assert_eq!(s.chosen_model(), None, "cycling a dead row picks nothing");
        assert!(s.is_noop(), "a dead model row cannot dirty the visit");
    }

    #[test]
    fn no_backend_persona_projects_the_operator_baseline() {
        // review-3 §3: a persona that declares no backend projects the operator
        // BASELINE (the revert target), not the outgoing persona's backend.
        let _g = GlobalSettingsGuard::acquire();
        let personas = vec![PersonaChoice {
            name: "alice".to_string(),
            cognition: None,
            tenacity: None,
            backend: None,
            crew: None,
        }];
        let mut s = PanelState::new(seed(None, personas, Tenacity::Standard, None, ""));
        s.cycle(1); // none → alice
        assert_eq!(
            s.projected_backend().as_deref(),
            Some("sol"),
            "no-backend persona reverts to the baseline"
        );
        assert_eq!(s.backend_provenance(), "base");
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
        s.down(); // model
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
        // The REAL exit path (close_outcome), not a hand-copied match: a save
        // happened but the loop was cancelled → Saved, never a posture summary.
        assert_eq!(
            close_outcome(false, &s),
            PanelOutcome::Saved {
                name: "keep".to_string()
            }
        );
        // And an Enter AFTER a lone :w with no dial/persona change: still Saved
        // (the save is reported; the noop "apply" stays silent), NOT
        // SavedAndApplied.
        assert_eq!(
            close_outcome(true, &s),
            PanelOutcome::Saved {
                name: "keep".to_string()
            }
        );
    }

    #[test]
    fn close_outcome_downgrades_a_noop_enter_and_applies_a_real_one() {
        // Adversarial-review findings on #1665: (1) the Enter downgrade
        // `applied && !is_noop()` must be exercised through the REAL exit path;
        // (2) the tenacity term of is_noop needs a tenacity-ONLY dirty case —
        // dropping `!self.tenacity.is_dirty()` from is_noop silently discarded
        // a tenacity-only edit (apply() skipped, Cancelled returned).
        use newt_core::cognition::{set_cli_cognition, CognitionOverride};
        let _g = GlobalSettingsGuard::acquire();
        set_cli_cognition(CognitionOverride::Unset);
        set_cli_tenacity(Tenacity::Standard);

        // Noop visit: Enter and Esc are indistinguishable — both Cancelled.
        let s = panel(None, two_personas(), Tenacity::Standard);
        assert_eq!(close_outcome(true, &s), PanelOutcome::Cancelled);
        assert_eq!(close_outcome(false, &s), PanelOutcome::Cancelled);
        assert_eq!(cli_tenacity(), Some(Tenacity::Standard), "nothing applied");

        // Tenacity-ONLY dirty + Enter → a real Applied, and the override is
        // actually written through apply().
        let mut t = panel(None, two_personas(), Tenacity::Standard);
        t.down(); // persona → model
        t.down(); // model → cognition
        t.down(); // cognition → tenacity
        t.cycle(1); // standard → insistent (dirty)
        let out = close_outcome(true, &t);
        assert_eq!(
            out,
            PanelOutcome::Applied {
                persona: PersonaAction::Keep,
                model: None,
            }
        );
        assert_eq!(
            cli_tenacity(),
            Some(Tenacity::Insistent),
            "the tenacity-only edit was applied, not silently discarded"
        );
        // The same dirty state WITHOUT the explicit apply stays Cancelled and
        // writes nothing further.
        set_cli_tenacity(Tenacity::Standard);
        assert_eq!(close_outcome(false, &t), PanelOutcome::Cancelled);
        assert_eq!(cli_tenacity(), Some(Tenacity::Standard));

        set_cli_cognition(CognitionOverride::Unset);
        set_cli_tenacity(Tenacity::Standard);
    }
}
