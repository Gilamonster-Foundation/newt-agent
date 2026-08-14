//! The **backend panel** (issue #1667) — behind the `rich-tui` feature.
//!
//! Bare `/backend` (and `/backends`, its alias) on a rich interactive terminal
//! opens this transient ratatui `Viewport::Inline` overlay: one surface to
//! **choose** the session backend, and to **edit / add / remove** the per-file
//! `~/.newt/backends/<name>.toml` drop-ins. It follows the house panel grammar
//! (#1665): `←`/`→` dial, `Enter` apply, `Esc` cancel silently, `:` for the
//! ex-command escapes — plus `e` edit, `a` add, `d` remove.
//!
//! ## Transaction semantics (config_panel review-3 §1 discipline)
//! - **Persistence is I/O-injected**: [`run`] takes `persist` and `remove`
//!   closures (the caller wires them to the setup wizard's crash-safe lock +
//!   plan machinery, #1660 — never a second write path). A failed write keeps
//!   the panel open with a visible status and mutates NOTHING.
//! - **The chooser pick is applied by the CALLER** through the exact
//!   `/backends <name>` / `/backend <kind>` slash path
//!   (`commands::model::{apply_backend_choice, apply_backend_kind}`), so the
//!   panel and the text commands share ONE set of switch semantics.
//! - **Removing the ACTIVE backend is refused** unless the same transaction
//!   also applies a different named selection: with the spinner dialed to
//!   another named backend, `:d <active>` closes the panel; the caller applies
//!   the new selection FIRST, then deletes the old drop-in.
//! - A no-op visit (untouched spinner, no file operation) closes silently —
//!   Enter and Esc are indistinguishable, exactly like `/psyche` (#1665).
//!
//! [`PanelState`] is pure (no terminal, no I/O) and unit-tested; the raw-mode
//! loop ([`run`]) mirrors `config_panel::run`.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use newt_core::BackendKind;

use crate::config_panel::{
    clamp_step, command_line, hint_line, make_terminal, render_panel, status_line, Dial, RowView,
};

/// What applying a chooser pick means — a NAMED `[[backends]]` entry (the
/// `/backends <name>` path) or a bare wire-kind toggle (the `/backend
/// <openai|ollama>` path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BackendSelection {
    Named(String),
    Kind(&'static str),
}

/// One spinner entry, built by the caller: every configured backend (the exact
/// set `/backends <name>` can switch to) plus the two kind fallbacks. Named
/// entries carry their form prefill; only file-backed ones are editable here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackendOption {
    pub name: String,
    pub selection: BackendSelection,
    /// Per-file drop-ins can be edited/removed from the panel; inline
    /// `config.toml` entries and the kind fallbacks cannot.
    pub editable: bool,
    pub kind: Option<BackendKind>,
    pub endpoint: String,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
    pub api_key_file: Option<String>,
}

impl BackendOption {
    /// The bare wire-kind fallback rows `/backend <openai|ollama>` supports.
    pub(crate) fn kind_fallback(kind: &'static str) -> Self {
        Self {
            name: kind.to_string(),
            selection: BackendSelection::Kind(kind),
            editable: false,
            kind: None,
            endpoint: String::new(),
            model: None,
            api_key_env: None,
            api_key_file: None,
        }
    }
}

/// Everything the CALLER resolves before the panel opens (same pattern as
/// `config_panel::PanelSeed`): the chooser options and which one is active.
pub(crate) struct PanelSeed {
    pub options: Vec<BackendOption>,
    /// Index into `options` of the backend the session currently resolves to
    /// (`None` when nothing matches, e.g. an env-shim endpoint).
    pub active: Option<usize>,
}

/// A validated edit-form result, handed to the injected `persist` closure. The
/// six form-managed fields; everything else in an existing drop-in (tiers,
/// serving, engine, provenance, …) is preserved by the persistence layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackendEdit {
    pub name: String,
    pub kind: Option<BackendKind>,
    pub endpoint: String,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
    pub api_key_file: Option<String>,
    /// `true` replaces the existing `<name>.toml` (edit); `false` must create
    /// it fresh (add — the plan commit refuses to clobber).
    pub replace: bool,
}

/// What one `persist` attempt did. `Saved.note` is the caller's summary line
/// (with the real written path), reported after the overlay clears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BackendSaveResult {
    Saved { note: String },
    Failed(String),
}

/// The panel's exit contract. There is deliberately no summary string for the
/// switch: the caller applies `apply` through the shared slash path and reports
/// from freshly-resolved runtime state (config_panel review-3 §2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PanelClose {
    /// The chooser pick to apply (None = nothing to switch — Esc, or Enter on
    /// an untouched spinner).
    pub apply: Option<BackendSelection>,
    /// The remove-active transaction (#1667): AFTER `apply` (a different named
    /// backend) succeeds, the caller deletes this drop-in.
    pub remove_after_apply: Option<String>,
    /// Summary lines for file operations that ALREADY happened in-loop
    /// (add/edit/remove) — printed by the caller, and its cue to re-resolve
    /// config even without a switch.
    pub changes: Vec<String>,
}

impl PanelClose {
    pub(crate) fn cancelled() -> Self {
        Self::default()
    }
}

/// The edit/add form's fields, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Kind,
    Url,
    Model,
    KeyEnv,
    KeyFile,
}

const FIELDS: [Field; 6] = [
    Field::Name,
    Field::Kind,
    Field::Url,
    Field::Model,
    Field::KeyEnv,
    Field::KeyFile,
];

/// The kind dial's ladder: auto (probe at connect) → the HTTP wire kinds. The
/// in-process `embedded` kind is deliberately absent — it has no endpoint and
/// belongs to `newt setup`.
const KIND_LADDER: [Option<BackendKind>; 3] =
    [None, Some(BackendKind::Ollama), Some(BackendKind::Openai)];

fn kind_label(kind: Option<BackendKind>) -> &'static str {
    match kind {
        None => "auto (probe)",
        Some(k) => k.label(),
    }
}

/// The edit/add form's working copy. Text fields are edited by typing directly
/// (so `:`/`e`/`a` stay typable in URLs and names); the kind field dials.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FormState {
    /// `None` = add; `Some(original name)` = edit. The name is FIXED while
    /// editing — renaming is add-new + `:d` old (two visible steps, no
    /// ambiguous move transaction).
    editing: Option<String>,
    sel: usize,
    name: String,
    kind_idx: usize,
    url: String,
    model: String,
    key_env: String,
    key_file: String,
}

impl FormState {
    fn add() -> Self {
        Self {
            editing: None,
            sel: 0,
            name: String::new(),
            kind_idx: 0,
            url: String::new(),
            model: String::new(),
            key_env: String::new(),
            key_file: String::new(),
        }
    }

    fn edit(opt: &BackendOption) -> Self {
        Self {
            editing: Some(opt.name.clone()),
            sel: 0,
            name: opt.name.clone(),
            kind_idx: KIND_LADDER.iter().position(|k| *k == opt.kind).unwrap_or(0),
            url: opt.endpoint.clone(),
            model: opt.model.clone().unwrap_or_default(),
            key_env: opt.api_key_env.clone().unwrap_or_default(),
            key_file: opt.api_key_file.clone().unwrap_or_default(),
        }
    }

    fn field_mut(&mut self) -> Option<&mut String> {
        match FIELDS[self.sel] {
            Field::Name => Some(&mut self.name),
            Field::Kind => None,
            Field::Url => Some(&mut self.url),
            Field::Model => Some(&mut self.model),
            Field::KeyEnv => Some(&mut self.key_env),
            Field::KeyFile => Some(&mut self.key_file),
        }
    }
}

/// Panel modes: the chooser, an ex-command line, or the edit/add form.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Choose,
    Command(String),
    Form(FormState),
}

/// The panel's working state. Pure: no terminal, no I/O; fully unit-testable.
/// Persistence is injected into [`PanelState::submit_form`] /
/// [`PanelState::run_command`] as closures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PanelState {
    options: Vec<BackendOption>,
    /// Index of the active backend at open (the `(active)` marker + the
    /// remove-active refusal).
    active: Option<usize>,
    /// Spinner position. Dirty = the operator touched it — a deliberate pick
    /// even back at the original position (same re-apply semantics as the
    /// psyche panel's model spinner).
    pick: Dial<usize>,
    mode: Mode,
    /// Transient status / error line (visible feedback — review-3 §1).
    status: Option<String>,
    /// Summary lines for file operations that already succeeded in-loop.
    changes: Vec<String>,
    /// Set by the remove-active transaction: delete this drop-in AFTER the
    /// caller applies the (different, named) pick.
    pending_remove: Option<String>,
}

impl PanelState {
    pub(crate) fn new(seed: PanelSeed) -> Self {
        let PanelSeed { options, active } = seed;
        let pick = active.unwrap_or(0).min(options.len().saturating_sub(1));
        Self {
            options,
            active,
            pick: Dial::Inherit(pick),
            mode: Mode::Choose,
            status: None,
            changes: Vec::new(),
            pending_remove: None,
        }
    }

    fn selected(&self) -> &BackendOption {
        &self.options[self.pick.value()]
    }

    fn in_command(&self) -> bool {
        matches!(self.mode, Mode::Command(_))
    }
    fn in_form(&self) -> bool {
        matches!(self.mode, Mode::Form(_))
    }

    fn named_index(&self, name: &str) -> Option<usize> {
        self.options
            .iter()
            .position(|o| matches!(&o.selection, BackendSelection::Named(n) if n == name))
    }

    pub(crate) fn cycle(&mut self, dir: i32) {
        self.status = None;
        self.pick
            .set(clamp_step(self.pick.value(), dir, self.options.len()));
    }

    /// True when applying would change nothing: the spinner was never touched.
    /// [`run`] downgrades Enter on a no-op panel to a silent close — bare
    /// `/backend` opens the panel, so browsing must never switch or report.
    pub(crate) fn is_noop(&self) -> bool {
        !self.pick.is_dirty()
    }

    // ── Chooser keys ─────────────────────────────────────────────────────
    pub(crate) fn begin_edit(&mut self) {
        self.status = None;
        let opt = self.selected().clone();
        match (&opt.selection, opt.editable) {
            (BackendSelection::Kind(_), _) => {
                self.status =
                    Some("the wire-kind toggles aren't editable — `a` adds a named backend".into());
            }
            (BackendSelection::Named(n), false) => {
                self.status = Some(format!(
                    "'{n}' lives inline in config.toml — edit that file (the panel edits \
                     ~/.newt/backends/ drop-ins)"
                ));
            }
            (BackendSelection::Named(_), true) => {
                self.mode = Mode::Form(FormState::edit(&opt));
            }
        }
    }

    pub(crate) fn begin_add(&mut self) {
        self.status = None;
        self.mode = Mode::Form(FormState::add());
    }

    /// `d`: prefill the typed confirm — the ex-command line `:d <name>` the
    /// operator must still Enter.
    pub(crate) fn begin_remove(&mut self) {
        self.status = None;
        match &self.selected().selection {
            BackendSelection::Named(n) => self.mode = Mode::Command(format!("d {n}")),
            BackendSelection::Kind(_) => {
                self.status = Some("select a named backend to remove".into());
            }
        }
    }

    pub(crate) fn begin_command(&mut self, prefill: &str) {
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
        self.mode = Mode::Choose;
    }

    // ── Form keys ────────────────────────────────────────────────────────
    fn form_nav(&mut self, dir: i32) {
        self.status = None;
        if let Mode::Form(form) = &mut self.mode {
            let n = FIELDS.len() as i32;
            form.sel = ((form.sel as i32 + dir).rem_euclid(n)) as usize;
        }
    }

    fn form_cycle(&mut self, dir: i32) {
        self.status = None;
        if let Mode::Form(form) = &mut self.mode {
            if FIELDS[form.sel] == Field::Kind {
                form.kind_idx = clamp_step(form.kind_idx, dir, KIND_LADDER.len());
            }
        }
    }

    fn form_input(&mut self, c: char) {
        let Mode::Form(form) = &mut self.mode else {
            return;
        };
        if FIELDS[form.sel] == Field::Name && form.editing.is_some() {
            self.status = Some(
                "the name is fixed while editing — `a` add under the new name, then :d the old"
                    .into(),
            );
            return;
        }
        if let Some(field) = form.field_mut() {
            if !c.is_control() {
                field.push(c);
                self.status = None;
            }
        }
    }

    fn form_backspace(&mut self) {
        if let Mode::Form(form) = &mut self.mode {
            if !(FIELDS[form.sel] == Field::Name && form.editing.is_some()) {
                if let Some(field) = form.field_mut() {
                    field.pop();
                }
            }
        }
    }

    fn cancel_form(&mut self) {
        self.status = None;
        self.mode = Mode::Choose;
    }

    /// Enter in the form: validate → persist (injected) → on success fold the
    /// saved entry back into the chooser; on ANY failure stay open with a
    /// visible status and mutate nothing (review-3 §1). Returns whether it saved.
    pub(crate) fn submit_form(
        &mut self,
        persist: &mut dyn FnMut(&BackendEdit) -> BackendSaveResult,
    ) -> bool {
        let Mode::Form(form) = &self.mode else {
            return false;
        };
        let edit = match validate_form(form, &self.options) {
            Ok(edit) => edit,
            Err(msg) => {
                self.status = Some(msg);
                return false;
            }
        };
        match persist(&edit) {
            BackendSaveResult::Saved { note } => {
                let opt = BackendOption {
                    name: edit.name.clone(),
                    selection: BackendSelection::Named(edit.name.clone()),
                    editable: true,
                    kind: edit.kind,
                    endpoint: edit.endpoint.clone(),
                    model: edit.model.clone(),
                    api_key_env: edit.api_key_env.clone(),
                    api_key_file: edit.api_key_file.clone(),
                };
                match self.named_index(&edit.name) {
                    Some(i) if edit.replace => self.options[i] = opt,
                    _ => self.insert_named_option(opt),
                }
                self.changes.push(note);
                self.status = Some(format!("saved backend '{}'", edit.name));
                self.mode = Mode::Choose;
                true
            }
            BackendSaveResult::Failed(err) => {
                self.status = Some(format!("save failed: {err}"));
                false
            }
        }
    }

    /// Insert a new named option just before the kind fallbacks, keeping the
    /// active marker and the spinner (with its dirtiness) on the same entries.
    fn insert_named_option(&mut self, opt: BackendOption) {
        let idx = self
            .options
            .iter()
            .position(|o| matches!(o.selection, BackendSelection::Kind(_)))
            .unwrap_or(self.options.len());
        self.options.insert(idx, opt);
        if let Some(a) = &mut self.active {
            if *a >= idx {
                *a += 1;
            }
        }
        let cur = self.pick.value();
        if cur >= idx {
            self.reposition(cur + 1);
        }
    }

    /// Drop option `idx`, shifting the active marker and spinner. A spinner
    /// that pointed AT the removed entry snaps back to the active entry (or 0)
    /// and loses its dirtiness — that pick no longer exists.
    fn remove_option(&mut self, idx: usize) {
        self.options.remove(idx);
        self.active = match self.active {
            Some(a) if a == idx => None,
            Some(a) if a > idx => Some(a - 1),
            other => other,
        };
        let cur = self.pick.value();
        if cur == idx {
            self.pick = Dial::Inherit(self.active.unwrap_or(0));
        } else if cur > idx {
            self.reposition(cur - 1);
        }
    }

    /// Move the spinner without changing whether it is dirty.
    fn reposition(&mut self, to: usize) {
        self.pick = match self.pick {
            Dial::Inherit(_) => Dial::Inherit(to),
            Dial::Set(_) => Dial::Set(to),
        };
    }

    // ── Ex-commands ──────────────────────────────────────────────────────
    /// Run the current ex-command, using `remove` for any file delete. Returns
    /// the close intent: `Some(true)` apply + close, `Some(false)` cancel +
    /// close, `None` stay open (with a status line).
    pub(crate) fn run_command(
        &mut self,
        remove: &mut dyn FnMut(&str) -> Result<String, String>,
    ) -> Option<bool> {
        let cmd = match &self.mode {
            Mode::Command(buf) => buf.trim().to_string(),
            _ => return None,
        };
        self.mode = Mode::Choose;
        let mut it = cmd.split_whitespace();
        let verb = it.next().unwrap_or("");
        let arg = it.next();
        match verb {
            "" => None,
            "q" => Some(false),
            "d" => self.remove_command(arg, remove),
            other => {
                self.status = Some(format!("unknown command ':{other}' (:d <name> | :q)"));
                None
            }
        }
    }

    fn remove_command(
        &mut self,
        name: Option<&str>,
        remove: &mut dyn FnMut(&str) -> Result<String, String>,
    ) -> Option<bool> {
        let Some(name) = name.map(str::trim).filter(|s| !s.is_empty()) else {
            self.status = Some("remove needs a name: :d <name>".to_string());
            return None;
        };
        let Some(idx) = self.named_index(name) else {
            self.status = Some(format!("no configured backend named '{name}'"));
            return None;
        };
        if !self.options[idx].editable {
            self.status = Some(format!(
                "'{name}' lives inline in config.toml — remove it there"
            ));
            return None;
        }
        if self.active == Some(idx) {
            // The active backend may only go together with a NEW selection
            // applied in the SAME transaction: spinner dialed to a different
            // named backend → close applying it, then the caller deletes this
            // file. Anything else is refused.
            let picked_other_named = self.pick.is_dirty()
                && self.pick.value() != idx
                && matches!(
                    self.options[self.pick.value()].selection,
                    BackendSelection::Named(_)
                );
            if !picked_other_named {
                self.status = Some(format!(
                    "'{name}' is the active backend — dial another named backend first; \
                     then :d {name} switches and removes it in one transaction"
                ));
                return None;
            }
            self.pending_remove = Some(name.to_string());
            return Some(true);
        }
        match remove(name) {
            Ok(note) => {
                self.remove_option(idx);
                self.changes.push(note);
                self.status = Some(format!("removed backend '{name}'"));
                None
            }
            Err(err) => {
                self.status = Some(format!("remove failed: {err}"));
                None
            }
        }
    }

    // ── Rendering ────────────────────────────────────────────────────────
    fn pick_label(&self) -> String {
        let opt = self.selected();
        let base = match &opt.selection {
            BackendSelection::Named(n) => n.clone(),
            BackendSelection::Kind(k) => format!("{k} — wire kind"),
        };
        if Some(self.pick.value()) == self.active {
            format!("{base} (active)")
        } else if self.pick.is_dirty() {
            format!("{base} (pending)")
        } else {
            base
        }
    }

    fn view_rows(&self) -> Vec<RowView> {
        match &self.mode {
            Mode::Form(form) => form_rows(form),
            _ => self.chooser_rows(),
        }
    }

    fn chooser_rows(&self) -> Vec<RowView> {
        let opt = self.selected();
        let is_kind = matches!(opt.selection, BackendSelection::Kind(_));
        let mut rows = vec![RowView {
            label: "backend",
            value: self.pick_label(),
            provenance: if is_kind {
                "session-only toggle".to_string()
            } else if opt.editable {
                "drop-in — e edits".to_string()
            } else {
                "inline config.toml".to_string()
            },
            selected: true,
            editable: true,
        }];
        if is_kind {
            rows.push(detail_row("kind", opt.name.clone()));
            rows.push(detail_row(
                "",
                format!("forces the {} wire protocol for this session", opt.name),
            ));
            rows.push(detail_row("", String::new()));
            rows.push(detail_row("", String::new()));
        } else {
            rows.push(detail_row("kind", kind_label(opt.kind).to_string()));
            rows.push(detail_row(
                "model",
                opt.model
                    .clone()
                    .unwrap_or_else(|| "(server decides)".to_string()),
            ));
            rows.push(detail_row("url", opt.endpoint.clone()));
            rows.push(detail_row(
                "auth",
                auth_label(opt.api_key_env.as_deref(), opt.api_key_file.as_deref()),
            ));
        }
        rows
    }
}

/// A read-only chooser detail row.
fn detail_row(label: &'static str, value: String) -> RowView {
    RowView {
        label,
        value,
        provenance: String::new(),
        selected: false,
        editable: false,
    }
}

fn auth_label(env: Option<&str>, file: Option<&str>) -> String {
    match (env, file) {
        (Some(e), Some(f)) => format!("env {e} · file {f}"),
        (Some(e), None) => format!("env {e}"),
        (None, Some(f)) => format!("file {f}"),
        (None, None) => "—".to_string(),
    }
}

fn form_rows(form: &FormState) -> Vec<RowView> {
    let cursor = |sel: bool, s: &str| {
        if sel {
            format!("{s}▏")
        } else {
            s.to_string()
        }
    };
    FIELDS
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let sel = form.sel == i;
            let (label, value) = match field {
                Field::Name => (
                    "name",
                    if form.editing.is_some() {
                        format!("{} (fixed)", form.name)
                    } else {
                        cursor(sel, &form.name)
                    },
                ),
                Field::Kind => ("kind", kind_label(KIND_LADDER[form.kind_idx]).to_string()),
                Field::Url => ("url", cursor(sel, &form.url)),
                Field::Model => ("model", cursor(sel, &form.model)),
                Field::KeyEnv => ("key env", cursor(sel, &form.key_env)),
                Field::KeyFile => ("key file", cursor(sel, &form.key_file)),
            };
            RowView {
                label,
                value,
                provenance: String::new(),
                selected: sel,
                // Every form row is editable (kind dials, text rows are typed
                // into) except the FIXED name while editing — that one renders
                // dim, like the read-only rows of the psyche panel.
                editable: !(*field == Field::Name && form.editing.is_some()),
            }
        })
        .collect()
}

fn valid_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}

fn none_if_empty(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Pure form validation: the checks that must hold BEFORE any I/O runs.
fn validate_form(form: &FormState, options: &[BackendOption]) -> Result<BackendEdit, String> {
    let name = form.name.trim().to_string();
    if name.is_empty() {
        return Err("backend needs a name".to_string());
    }
    if !name.chars().all(valid_name_char) {
        return Err("name may use letters, digits, '.', '-', '_' only".to_string());
    }
    if form.editing.is_none()
        && options
            .iter()
            .any(|o| matches!(&o.selection, BackendSelection::Named(n) if *n == name))
    {
        return Err(format!("'{name}' already exists — select it and press e"));
    }
    let url = form.url.trim().to_string();
    if url.is_empty() {
        return Err("backend needs a url (e.g. http://host:11434)".to_string());
    }
    match reqwest::Url::parse(&url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => {}
        _ => return Err(format!("invalid url '{url}' — needs http:// or https://")),
    }
    let key_env = form.key_env.trim();
    if key_env.chars().any(|c| c.is_whitespace() || c == '=') {
        return Err("api-key env must be a bare variable name (e.g. OPENAI_API_KEY)".to_string());
    }
    Ok(BackendEdit {
        name,
        kind: KIND_LADDER[form.kind_idx],
        endpoint: url,
        model: none_if_empty(&form.model),
        api_key_env: none_if_empty(&form.key_env),
        api_key_file: none_if_empty(&form.key_file),
        replace: form.editing.is_some(),
    })
}

/// Bordered block (2) + up to six rows + a hint/command/status row.
const PANEL_HEIGHT: u16 = 10;

fn draw(f: &mut ratatui::Frame, state: &PanelState) {
    let title = match &state.mode {
        Mode::Form(form) if form.editing.is_some() => " backend — edit ",
        Mode::Form(_) => " backend — add ",
        _ => " backend — chooser ",
    };
    let bottom = if let Mode::Command(buf) = &state.mode {
        command_line(buf)
    } else if let Some(status) = &state.status {
        status_line(status)
    } else if matches!(state.mode, Mode::Form(_)) {
        hint_line("↑↓ field · type to edit · ←→ kind · Enter save · Esc back")
    } else {
        hint_line("←→ choose · Enter apply · e edit · a add · d remove · Esc cancel")
    };
    render_panel(f, title, &state.view_rows(), bottom, 9, 34);
}

/// The panel's exit contract, factored out of the raw-mode loop so it is
/// unit-testable without a TTY (same pattern as `config_panel::close_outcome`):
/// an explicit apply reports the pick ONLY when the spinner was touched — the
/// noop downgrade lives here — and the remove-active transaction rides along
/// only when its precondition (a different named pick) still holds.
fn close_outcome(applied: bool, state: &PanelState) -> PanelClose {
    let apply = if applied && !state.is_noop() {
        Some(state.selected().selection.clone())
    } else {
        None
    };
    let remove_after_apply = match (&apply, &state.pending_remove) {
        (Some(BackendSelection::Named(new)), Some(old)) if new != old => Some(old.clone()),
        _ => None,
    };
    PanelClose {
        apply,
        remove_after_apply,
        changes: state.changes.clone(),
    }
}

/// Open the panel, drive its raw-mode inline event loop, and return the close
/// contract. `persist` / `remove` are the ONLY filesystem I/O — injected so a
/// failed write keeps the panel open with a visible status and mutates nothing
/// (review-3 §1). The chooser pick is NOT applied here: the caller routes it
/// through the shared `/backends` / `/backend` slash path and then replicates
/// the loop's post-command refresh.
pub(crate) fn run(
    seed: PanelSeed,
    mut persist: impl FnMut(&BackendEdit) -> BackendSaveResult,
    mut remove: impl FnMut(&str) -> Result<String, String>,
) -> io::Result<PanelClose> {
    if seed.options.is_empty() {
        return Ok(PanelClose::cancelled());
    }
    let mut state = PanelState::new(seed);
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
                    KeyCode::Char(c) if !ctrl => state.command_char(c),
                    KeyCode::Backspace => state.command_backspace(),
                    KeyCode::Esc => state.cancel_command(),
                    KeyCode::Enter => {
                        if let Some(apply) = state.run_command(&mut remove) {
                            applied = apply;
                            break;
                        }
                    }
                    _ => {}
                }
            } else if state.in_form() {
                match key.code {
                    KeyCode::Up => state.form_nav(-1),
                    KeyCode::Down => state.form_nav(1),
                    KeyCode::Left => state.form_cycle(-1),
                    KeyCode::Right => state.form_cycle(1),
                    KeyCode::Backspace => state.form_backspace(),
                    KeyCode::Enter => {
                        state.submit_form(&mut persist);
                    }
                    KeyCode::Esc => state.cancel_form(),
                    KeyCode::Char(c) if !ctrl => state.form_input(c),
                    _ => {}
                }
            } else {
                match key.code {
                    KeyCode::Left => state.cycle(-1),
                    KeyCode::Right => state.cycle(1),
                    KeyCode::Char('e') => state.begin_edit(),
                    KeyCode::Char('a') => state.begin_add(),
                    KeyCode::Char('d') => state.begin_remove(),
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
    Ok(close_outcome(applied, &state))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str, editable: bool) -> BackendOption {
        BackendOption {
            name: name.to_string(),
            selection: BackendSelection::Named(name.to_string()),
            editable,
            kind: Some(BackendKind::Ollama),
            endpoint: format!("http://{name}:11434"),
            model: Some("qwen3:30b".to_string()),
            api_key_env: None,
            api_key_file: None,
        }
    }

    /// dgx1 (active, file-backed) · gnuc (file-backed) · relic (inline) + the
    /// two kind fallbacks.
    fn panel() -> PanelState {
        PanelState::new(PanelSeed {
            options: vec![
                named("dgx1", true),
                named("gnuc", true),
                named("relic", false),
                BackendOption::kind_fallback("ollama"),
                BackendOption::kind_fallback("openai"),
            ],
            active: Some(0),
        })
    }

    fn ok_persist() -> impl FnMut(&BackendEdit) -> BackendSaveResult {
        |edit: &BackendEdit| BackendSaveResult::Saved {
            note: format!("saved backend '{}'", edit.name),
        }
    }

    fn ok_remove() -> impl FnMut(&str) -> Result<String, String> {
        |name: &str| Ok(format!("removed backend '{name}'"))
    }

    /// Drive the state into the form and type `text` into the selected field.
    fn type_text(s: &mut PanelState, text: &str) {
        for c in text.chars() {
            s.form_input(c);
        }
    }

    #[test]
    fn untouched_spinner_is_a_noop_and_enter_closes_silently() {
        let s = panel();
        assert!(s.is_noop(), "fresh panel: nothing to apply");
        // Enter and Esc are indistinguishable on a noop visit.
        assert_eq!(close_outcome(true, &s), PanelClose::cancelled());
        assert_eq!(close_outcome(false, &s), PanelClose::cancelled());
    }

    #[test]
    fn spinner_dials_named_then_kind_fallbacks_with_markers_and_clamp() {
        let mut s = panel();
        assert!(
            s.pick_label().contains("dgx1") && s.pick_label().contains("(active)"),
            "opens on the active backend: {}",
            s.pick_label()
        );
        s.cycle(1); // → gnuc
        assert!(!s.is_noop(), "a touched spinner is a pick");
        assert!(s.pick_label().contains("gnuc") && s.pick_label().contains("(pending)"));
        assert_eq!(
            close_outcome(true, &s).apply,
            Some(BackendSelection::Named("gnuc".to_string()))
        );
        // Through relic and onto the kind fallbacks…
        s.cycle(1);
        s.cycle(1);
        assert_eq!(
            close_outcome(true, &s).apply,
            Some(BackendSelection::Kind("ollama"))
        );
        // …and the right edge clamps (no wrap).
        s.cycle(1);
        s.cycle(1);
        s.cycle(1);
        assert_eq!(
            close_outcome(true, &s).apply,
            Some(BackendSelection::Kind("openai"))
        );
        // Cancelling a dirty spinner applies nothing.
        assert_eq!(close_outcome(false, &s), PanelClose::cancelled());
    }

    #[test]
    fn touched_back_to_active_is_still_a_deliberate_reapply() {
        // Same semantics as the psyche model spinner (#1666): cycling away and
        // back is a deliberate same-value re-apply, not a noop.
        let mut s = panel();
        s.cycle(1);
        s.cycle(-1);
        assert!(!s.is_noop());
        assert_eq!(
            close_outcome(true, &s).apply,
            Some(BackendSelection::Named("dgx1".to_string()))
        );
    }

    #[test]
    fn edit_form_prefills_and_saves_through_injected_persist() {
        let mut s = panel();
        s.cycle(1); // → gnuc
        s.begin_edit();
        let Mode::Form(form) = &s.mode else {
            panic!("edit should open the form");
        };
        assert_eq!(form.editing.as_deref(), Some("gnuc"));
        assert_eq!(form.url, "http://gnuc:11434");
        assert_eq!(form.model, "qwen3:30b");
        // ↓↓↓ to the model field, clear it, type a new one.
        s.form_nav(1); // kind
        s.form_nav(1); // url
        s.form_nav(1); // model
        for _ in 0.."qwen3:30b".len() {
            s.form_backspace();
        }
        type_text(&mut s, "llama3.1:8b");
        let mut seen: Vec<BackendEdit> = Vec::new();
        let mut persist = |edit: &BackendEdit| {
            seen.push(edit.clone());
            BackendSaveResult::Saved {
                note: "saved backend 'gnuc' → /tmp/gnuc.toml".to_string(),
            }
        };
        assert!(s.submit_form(&mut persist));
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].name, "gnuc");
        assert_eq!(seen[0].model.as_deref(), Some("llama3.1:8b"));
        assert!(seen[0].replace, "editing replaces the existing drop-in");
        // The chooser folded the save back in and recorded the change.
        assert_eq!(s.mode, Mode::Choose);
        assert_eq!(
            s.options[1].model.as_deref(),
            Some("llama3.1:8b"),
            "chooser reflects the edit"
        );
        assert_eq!(s.changes, vec!["saved backend 'gnuc' → /tmp/gnuc.toml"]);
    }

    #[test]
    fn add_form_validates_before_any_io_runs() {
        let mut called = 0usize;
        let mut persist = |_: &BackendEdit| {
            called += 1;
            BackendSaveResult::Saved {
                note: String::new(),
            }
        };
        let cases: &[(&str, &str, &str)] = &[
            // (name, url, expected status fragment)
            ("", "http://x:1", "needs a name"),
            ("bad name", "http://x:1", "letters, digits"),
            ("dgx1", "http://x:1", "already exists"),
            ("fresh", "", "needs a url"),
            ("fresh", "host:11434", "http:// or https://"),
        ];
        for (name, url, want) in cases {
            let mut s = panel();
            s.begin_add();
            type_text(&mut s, name);
            s.form_nav(1); // kind
            s.form_nav(1); // url
            type_text(&mut s, url);
            assert!(!s.submit_form(&mut persist), "case {name:?} {url:?}");
            assert!(
                s.status.as_deref().unwrap_or("").contains(want),
                "case {name:?} {url:?} → {:?}",
                s.status
            );
            assert!(matches!(s.mode, Mode::Form(_)), "stays open to fix it");
        }
        // A whitespace-y api-key env is refused too.
        let mut s = panel();
        s.begin_add();
        type_text(&mut s, "fresh");
        s.form_nav(1);
        s.form_nav(1);
        type_text(&mut s, "http://x:1");
        s.form_nav(1); // model
        s.form_nav(1); // key env
        type_text(&mut s, "NOT AVAR");
        assert!(!s.submit_form(&mut persist));
        assert!(s.status.as_deref().unwrap().contains("bare variable name"));
        assert_eq!(called, 0, "validation failures never reach the disk");
    }

    #[test]
    fn failed_persist_keeps_the_form_open_and_mutates_nothing() {
        // review-3 §1: a failed write keeps the panel open with a visible
        // status; options, changes, and the runtime stay untouched.
        let mut s = panel();
        let before = s.options.clone();
        s.begin_add();
        type_text(&mut s, "newbie");
        s.form_nav(1); // kind
        s.form_nav(1); // url
        type_text(&mut s, "http://newbie:8000");
        let mut persist = |_: &BackendEdit| BackendSaveResult::Failed("disk full".to_string());
        assert!(!s.submit_form(&mut persist));
        assert!(matches!(s.mode, Mode::Form(_)), "stays open for a retry");
        assert!(s.status.as_deref().unwrap().contains("disk full"));
        assert_eq!(s.options, before, "no phantom chooser entry");
        assert!(s.changes.is_empty(), "nothing recorded as changed");
        assert_eq!(close_outcome(false, &s), PanelClose::cancelled());
    }

    #[test]
    fn add_saves_a_new_entry_before_the_kind_fallbacks() {
        let mut s = panel();
        s.begin_add();
        type_text(&mut s, "fresh");
        s.form_nav(1);
        s.form_nav(1);
        type_text(&mut s, "https://fresh.example");
        let mut persist = ok_persist();
        assert!(s.submit_form(&mut persist));
        assert_eq!(s.mode, Mode::Choose);
        assert_eq!(s.options[3].name, "fresh", "inserted after the named set");
        assert!(s.options[3].editable, "a fresh drop-in is editable");
        assert!(matches!(
            s.options[4].selection,
            BackendSelection::Kind("ollama")
        ));
        assert_eq!(s.changes.len(), 1);
        // The active marker did not drift.
        assert!(s.pick_label().contains("dgx1") && s.pick_label().contains("(active)"));
    }

    #[test]
    fn name_is_fixed_while_editing_but_free_while_adding() {
        let mut s = panel();
        s.begin_edit(); // dgx1
        type_text(&mut s, "x");
        let Mode::Form(form) = &s.mode else {
            panic!("form")
        };
        assert_eq!(form.name, "dgx1", "edit cannot rename");
        assert!(s.status.as_deref().unwrap().contains("fixed"));
        let mut a = panel();
        a.begin_add();
        type_text(&mut a, "brand-new");
        let Mode::Form(form) = &a.mode else {
            panic!("form")
        };
        assert_eq!(form.name, "brand-new");
    }

    #[test]
    fn edit_is_blocked_for_kind_fallbacks_and_inline_entries() {
        let mut s = panel();
        s.cycle(1); // gnuc
        s.cycle(1); // relic (inline)
        s.begin_edit();
        assert!(matches!(s.mode, Mode::Choose), "inline entry: no form");
        assert!(s.status.as_deref().unwrap().contains("inline"));
        s.cycle(1); // ollama kind fallback
        s.begin_edit();
        assert!(matches!(s.mode, Mode::Choose));
        assert!(s.status.as_deref().unwrap().contains("aren't editable"));
    }

    #[test]
    fn d_key_prefills_the_typed_confirm() {
        let mut s = panel();
        s.cycle(1); // gnuc
        s.begin_remove();
        assert_eq!(s.mode, Mode::Command("d gnuc".to_string()));
        // On a kind fallback there is nothing to remove.
        let mut k = panel();
        k.cycle(1);
        k.cycle(1);
        k.cycle(1); // ollama
        k.begin_remove();
        assert!(matches!(k.mode, Mode::Choose));
        assert!(k.status.as_deref().unwrap().contains("named backend"));
    }

    #[test]
    fn remove_nonactive_deletes_via_injected_closure_and_stays_open() {
        let mut s = panel();
        let mut removed: Vec<String> = Vec::new();
        let mut remove = |name: &str| {
            removed.push(name.to_string());
            Ok(format!("removed backend '{name}'"))
        };
        s.begin_command("d gnuc");
        assert_eq!(s.run_command(&mut remove), None, "stays open");
        assert_eq!(removed, vec!["gnuc"]);
        assert!(s.named_index("gnuc").is_none(), "chooser entry gone");
        assert_eq!(s.changes, vec!["removed backend 'gnuc'"]);
        // Active marker (dgx1, index 0) survives the shift.
        assert!(s.pick_label().contains("dgx1") && s.pick_label().contains("(active)"));
    }

    #[test]
    fn removing_the_entry_under_a_dirty_cursor_resets_the_pick() {
        let mut s = panel();
        s.cycle(1); // dial to gnuc (dirty)
        let mut remove = ok_remove();
        s.begin_command("d gnuc");
        assert_eq!(s.run_command(&mut remove), None);
        assert!(
            s.is_noop(),
            "the removed pick no longer exists — back to a clean spinner"
        );
        assert!(s.pick_label().contains("dgx1") && s.pick_label().contains("(active)"));
    }

    #[test]
    fn remove_failure_keeps_the_option_and_shows_why() {
        let mut s = panel();
        let mut remove = |_: &str| Err("permission denied".to_string());
        s.begin_command("d gnuc");
        assert_eq!(s.run_command(&mut remove), None);
        assert!(s.named_index("gnuc").is_some(), "nothing dropped");
        assert!(s.changes.is_empty());
        assert!(s.status.as_deref().unwrap().contains("permission denied"));
    }

    #[test]
    fn remove_refuses_the_active_backend_without_a_new_selection() {
        let mut s = panel();
        let called = std::cell::Cell::new(false);
        let mut remove = |_: &str| {
            called.set(true);
            Ok(String::new())
        };
        s.begin_command("d dgx1");
        assert_eq!(s.run_command(&mut remove), None, "refused, stays open");
        assert!(!called.get(), "the file is never touched");
        assert!(s.status.as_deref().unwrap().contains("active backend"));
        // A dirty pick on a KIND fallback is not a valid replacement either.
        s.cycle(1);
        s.cycle(1);
        s.cycle(1); // → ollama kind
        s.begin_command("d dgx1");
        assert_eq!(s.run_command(&mut remove), None);
        assert!(!called.get());
        assert!(s.pending_remove.is_none());
    }

    #[test]
    fn remove_active_with_a_dirty_named_selection_closes_as_one_transaction() {
        let mut s = panel();
        s.cycle(1); // dial to gnuc (a different NAMED backend)
        let mut called = false;
        let mut remove = |_: &str| {
            called = true;
            Ok(String::new())
        };
        s.begin_command("d dgx1");
        assert_eq!(
            s.run_command(&mut remove),
            Some(true),
            "closes applying the new selection"
        );
        assert!(
            !called,
            "the delete is deferred: the caller applies the switch FIRST, then removes"
        );
        // The REAL exit path carries both halves of the transaction.
        let close = close_outcome(true, &s);
        assert_eq!(
            close.apply,
            Some(BackendSelection::Named("gnuc".to_string()))
        );
        assert_eq!(close.remove_after_apply.as_deref(), Some("dgx1"));
    }

    #[test]
    fn ex_commands_validate_visibly() {
        let mut s = panel();
        let mut remove = ok_remove();
        for (cmd, want) in [
            ("d", "needs a name"),
            ("d ghost", "no configured backend"),
            ("d relic", "inline"),
            ("banana", "unknown command"),
        ] {
            s.begin_command(cmd);
            assert_eq!(s.run_command(&mut remove), None, "{cmd:?} stays open");
            assert!(
                s.status.as_deref().unwrap().contains(want),
                "{cmd:?} → {:?}",
                s.status
            );
        }
        s.begin_command("q");
        assert_eq!(s.run_command(&mut remove), Some(false), ":q cancels");
    }

    #[test]
    fn file_changes_survive_a_cancel_so_the_caller_still_refreshes() {
        // An add/remove already happened on disk; Esc must still hand the
        // caller the change notes (its cue to re-resolve config), while
        // applying nothing.
        let mut s = panel();
        let mut remove = ok_remove();
        s.begin_command("d gnuc");
        s.run_command(&mut remove);
        let close = close_outcome(false, &s);
        assert_eq!(close.apply, None);
        assert_eq!(close.changes, vec!["removed backend 'gnuc'"]);
    }

    #[test]
    fn chooser_rows_render_the_selected_backend_and_kind_fallback_details() {
        let mut s = panel();
        let rows = s.chooser_rows();
        assert_eq!(rows[0].label, "backend");
        assert!(rows[0].value.contains("dgx1"));
        assert!(rows
            .iter()
            .any(|r| r.label == "url" && r.value == "http://dgx1:11434"));
        assert!(rows.iter().any(|r| r.label == "auth" && r.value == "—"));
        // Kind fallback: no url/model/auth pretence, a session-only note.
        s.cycle(1);
        s.cycle(1);
        s.cycle(1); // ollama
        let rows = s.chooser_rows();
        assert!(rows[0].value.contains("wire kind"));
        assert_eq!(rows[0].provenance, "session-only toggle");
        assert!(!rows.iter().any(|r| r.label == "url"));
    }
}
