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
//! ## Source of truth — the panel is an EDITOR, never a second authority
//! The chooser must not become a competing copy of session posture. Each layer
//! keeps its existing owner; the panel only *edits* them through that owner:
//!
//! | # | Layer | Canonical owner | The panel's part |
//! |---|-------|-----------------|------------------|
//! | a | current backend (this session) | the process env + `newt_core::Config::resolve()` → `chat::refresh_backend`; written ONLY by `commands::model::apply_backend_choice` / `apply_backend_kind` | hands the pick to that function; never sets `NEWT_PROVIDER` itself |
//! | b | current model | `commands::model::apply_model_choice` / the adopt path (`NEWT_DGX_MODEL`) | never touched here — the form's `model` is the *drop-in's declared* model, config, not session state |
//! | c | persisted default | `config.toml` `default_backend` (via `Config::with_default_backend`) and `~/.newt/settings.toml` `provider` (via `settings::record_provider`, inside `apply_backend_choice`) | writes them only through those two owners: `setup::persist_panel_backend` / `remove_panel_backend` for the files, `apply_backend_choice` for the settings pin |
//! | d | conversation-scoped override | the PosturePin work (#1684, separate PR) | out of scope here; the panel neither reads nor writes it |
//! | e | in-panel selection (dirty, uncommitted) | [`PanelState::pick`] — a `Dial`, alive only while the overlay is open | discarded on Esc; on Enter it is handed to (a) and never re-read |
//!
//! Precedence when they disagree, most specific first: (e) only at the moment
//! of apply → (d) → (a) `$NEWT_PROVIDER` → (c) `settings.toml` provider →
//! (c) `config.toml` `default_backend` → newt's discovery heuristics. That is
//! `Config::select_configured_backend` + `chat::resolve_backend_choice`,
//! unchanged by this panel.
//!
//! Consequently [`PanelState`]'s `options` / `active` are a **read-only snapshot
//! taken by the caller before the overlay opens** — a view, not a store. It is
//! never consulted after close: the caller re-resolves `Config` and re-derives
//! the session from it, so a panel-local value can never disagree with the
//! resolved one. Saved edits are folded back into the snapshot purely so the
//! still-open chooser shows what the operator just wrote.
//!
//! [`PanelState`] is pure (no terminal, no I/O) and unit-tested; the raw-mode
//! loop ([`run`]) mirrors `config_panel::run`.

use crate::panel::Key;
use std::io;

use newt_core::BackendKind;

use crate::config_panel::{
    clamp_step, command_line, hint_line, render_panel, status_line, Dial, RowView,
};

/// What applying a chooser pick means — a NAMED `[[backends]]` entry (the
/// `/backends <name>` path) or a bare wire-kind toggle (the `/backend
/// <openai|ollama>` path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BackendSelection {
    Named(String),
    Kind(&'static str),
}

/// WHERE a chooser row's definition lives — which decides whether this panel may
/// edit it, and (when it may not) what to tell the operator instead. Data, not a
/// bare `editable: bool`, so the refusal can name the real reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackendSource {
    /// `~/.newt/backends/<name>.toml` — the only rows this panel writes.
    UserDropIn,
    /// A user drop-in that ALSO has a same-named inline `[[backends]]` entry in
    /// `config.toml`. Still editable, but the merge re-inherits `api_key_*` /
    /// `tiers` the drop-in omits, so clearing an auth field here can come back
    /// (review §4) — the panel says so on save.
    UserDropInOverInline,
    /// An inline `[[backends]]` entry in `config.toml` — read-only here.
    Inline,
    /// A `<project>/.newt/backends/<name>.toml` drop-in wins over the user's
    /// (`Config::merge_disk_backends` merges the project dir LAST). Editing or
    /// removing the user file would be a silent no-op / a phantom delete
    /// (review §3), so the panel refuses both.
    ShadowedByProject,
    /// A bare wire-kind toggle row (`/backend openai|ollama`).
    KindToggle,
}

impl BackendSource {
    /// Only a user drop-in this panel actually owns may be edited/removed.
    fn editable(self) -> bool {
        matches!(self, Self::UserDropIn | Self::UserDropInOverInline)
    }

    /// The chooser row's provenance column.
    fn provenance(self) -> &'static str {
        match self {
            Self::UserDropIn => "drop-in — e edits",
            Self::UserDropInOverInline => "drop-in + inline entry",
            Self::Inline => "inline config.toml",
            Self::ShadowedByProject => "shadowed by project config",
            Self::KindToggle => "session-only toggle",
        }
    }

    /// Where this row's configuration lives, for a delete to name.
    ///
    /// A drop-in file and a `config.toml` entry are removed from different
    /// places, and an operator about to lose one should be told which.
    fn describe(self) -> &'static str {
        match self {
            Self::UserDropIn => "~/.newt/backends/",
            Self::UserDropInOverInline => "~/.newt/backends/ (shadowing an inline entry)",
            Self::Inline => "config.toml",
            Self::ShadowedByProject => "a project .newt/backends drop-in",
            Self::KindToggle => "this session",
        }
    }

    /// Why this row cannot be edited/removed from here.
    fn refusal(self, name: &str) -> String {
        match self {
            Self::UserDropIn | Self::UserDropInOverInline => String::new(),
            Self::Inline => format!(
                "'{name}' lives inline in config.toml — edit that file (the panel edits \
                 ~/.newt/backends/ drop-ins)"
            ),
            Self::ShadowedByProject => format!(
                "'{name}' is shadowed by a project .newt/backends drop-in — edit that file \
                 (a change here would not take effect)"
            ),
            Self::KindToggle => {
                "the wire-kind toggles aren't editable — `a` adds a named backend".to_string()
            }
        }
    }
}

/// One spinner entry, built by the caller: every configured backend (the exact
/// set `/backends <name>` can switch to) plus the two kind fallbacks. Named
/// entries carry their form prefill; only the user drop-ins this panel owns are
/// editable here (see [`BackendSource`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackendOption {
    pub name: String,
    pub selection: BackendSelection,
    pub source: BackendSource,
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
            source: BackendSource::KindToggle,
            kind: None,
            endpoint: String::new(),
            model: None,
            api_key_env: None,
            api_key_file: None,
        }
    }

    fn editable(&self) -> bool {
        self.source.editable()
    }
}

/// Everything the CALLER resolves before the panel opens (same pattern as
/// `config_panel::PanelSeed`): the chooser options and which one is active.
pub(crate) struct PanelSeed {
    pub options: Vec<BackendOption>,
    /// Index into `options` of the backend the session currently resolves to
    /// (`None` when nothing matches, e.g. an env-shim endpoint).
    pub active: Option<usize>,
    /// `config.toml`'s `default_backend` — the DURABLE pointer, which diverges
    /// from `active` whenever `$NEWT_PROVIDER` / a restored settings pin names
    /// another backend. Removing it needs the same one-transaction treatment as
    /// removing the active backend, or the next headless run hard-errors on a
    /// dangling pointer (review §2/§7/§11).
    pub default_backend: Option<String>,
}

/// Which form fields the operator ACTUALLY changed. The persistence layer
/// overlays only these onto the file it re-reads at save time, so an untouched
/// field can neither revert a concurrent writer (review §6) nor silently drop a
/// value the form cannot express — e.g. `kind = "anthropic"` (review §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct DirtyFields {
    pub kind: bool,
    pub endpoint: bool,
    pub model: bool,
    pub api_key_env: bool,
    pub api_key_file: bool,
}

impl DirtyFields {
    /// An ADD writes a whole new file: every field is the operator's.
    fn all() -> Self {
        Self {
            kind: true,
            endpoint: true,
            model: true,
            api_key_env: true,
            api_key_file: true,
        }
    }

    fn any(self) -> bool {
        self.kind || self.endpoint || self.model || self.api_key_env || self.api_key_file
    }
}

/// A validated edit-form result, handed to the injected `persist` closure. The
/// six form-managed fields plus `dirty` (which of them the operator touched);
/// everything else in an existing drop-in — unmanaged fields, comments, and any
/// key newt does not model — is preserved by the persistence layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackendEdit {
    pub name: String,
    pub kind: Option<BackendKind>,
    pub endpoint: String,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
    pub api_key_file: Option<String>,
    pub dirty: DirtyFields,
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
///
/// EVERY HTTP kind must appear here: [`FormState::edit`] resolves the dial by
/// `position()`, so a kind missing from the ladder would prefill at index 0
/// ("auto") and silently downgrade a PINNED kind to probe-at-connect on an
/// unrelated save — see the anthropic regression test below. A kind that is
/// still not representable ([`BackendKind::Embedded`]) fails CLOSED in
/// [`PanelState::begin_edit`]; and as a second line of defence the save
/// overlays the `kind` key only when the operator actually MOVED the dial
/// ([`FormState::dirty`], review §1/§6), so an untouched kind is never written
/// at all.
const KIND_LADDER: [Option<BackendKind>; 4] = [
    None,
    Some(BackendKind::Ollama),
    Some(BackendKind::Openai),
    Some(BackendKind::Anthropic),
];

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
    /// The prefill, kept verbatim so save time can tell which fields the
    /// operator actually changed (review §1/§6). Default for an add.
    original: FormValues,
}

/// The five overlayable form values, as prefilled. `kind` is kept as the DIAL
/// POSITION rather than the value: "the operator moved the dial" is the honest
/// test of intent, and it stays honest even if a kind ever prefills off-ladder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FormValues {
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
            original: FormValues::default(),
        }
    }

    fn edit(opt: &BackendOption) -> Self {
        // `begin_edit` refuses any kind the ladder cannot represent, so this
        // position() always hits; the fallback would only ever be reached by a
        // future caller, and `dirty` keeps even that case from writing a kind
        // the operator never dialed.
        let kind_idx = KIND_LADDER.iter().position(|k| *k == opt.kind).unwrap_or(0);
        Self {
            editing: Some(opt.name.clone()),
            sel: 0,
            name: opt.name.clone(),
            kind_idx,
            url: opt.endpoint.clone(),
            model: opt.model.clone().unwrap_or_default(),
            key_env: opt.api_key_env.clone().unwrap_or_default(),
            key_file: opt.api_key_file.clone().unwrap_or_default(),
            original: FormValues {
                kind_idx,
                url: opt.endpoint.clone(),
                model: opt.model.clone().unwrap_or_default(),
                key_env: opt.api_key_env.clone().unwrap_or_default(),
                key_file: opt.api_key_file.clone().unwrap_or_default(),
            },
        }
    }

    fn kind(&self) -> Option<BackendKind> {
        KIND_LADDER.get(self.kind_idx).copied().flatten()
    }

    /// Which fields differ from the prefill (an ADD dirties everything).
    fn dirty(&self) -> DirtyFields {
        if self.editing.is_none() {
            return DirtyFields::all();
        }
        let changed = |now: &str, before: &str| now.trim() != before.trim();
        DirtyFields {
            kind: self.kind_idx != self.original.kind_idx,
            endpoint: changed(&self.url, &self.original.url),
            model: changed(&self.model, &self.original.model),
            api_key_env: changed(&self.key_env, &self.original.key_env),
            api_key_file: changed(&self.key_file, &self.original.key_file),
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
    Form(Box<FormState>),
    /// A destructive action, waiting to be confirmed.
    ///
    /// **Every delete asks first.** Before this, `d` prefilled the ex-command
    /// `d <name>` and Enter ran it — which is a confirmation only in the sense
    /// that a keystroke separates you from the deletion. It named no
    /// consequence, and the row it would remove was the one already under the
    /// cursor, so the prefill read as a label rather than as a question.
    Confirm(Confirm),
}

/// A pending destructive action and the question that guards it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Confirm {
    /// The question, naming the target and what goes with it. Built at the
    /// moment of asking, so it can say what THIS deletion costs rather than
    /// what deletions cost in general.
    prompt: String,
    action: Pending,
}

/// What a confirmed answer performs.
///
/// One variant today. It is an enum rather than a bare name because the panels
/// after this one — conversations, personas — delete different things, and the
/// confirm should be the shape they can adopt rather than a fourth thing each
/// invents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Pending {
    RemoveBackend(String),
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
    /// `config.toml`'s durable `default_backend` — removing it needs the same
    /// one-transaction switch as removing the active backend.
    default_backend: Option<String>,
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
        let PanelSeed {
            options,
            active,
            default_backend,
        } = seed;
        let pick = active.unwrap_or(0).min(options.len().saturating_sub(1));
        Self {
            options,
            active,
            default_backend,
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
        if !opt.editable() {
            self.status = Some(opt.source.refusal(&opt.name));
        } else if !KIND_LADDER.contains(&opt.kind) {
            // A kind the dial cannot represent (today: `embedded`, which has no
            // endpoint and belongs to `newt setup`) must REFUSE the form rather
            // than open one whose kind row silently reads back as something
            // else.
            let label = kind_label(opt.kind);
            let name = &opt.name;
            self.status = Some(format!(
                "'{name}' is a {label} backend — the panel edits http backends only; \
                 edit ~/.newt/backends/{name}.toml"
            ));
        } else {
            self.mode = Mode::Form(Box::new(FormState::edit(&opt)));
        }
    }

    pub(crate) fn begin_add(&mut self) {
        self.status = None;
        self.mode = Mode::Form(Box::new(FormState::add()));
    }

    /// Ask before removing the selected backend.
    ///
    /// **Every refusal is evaluated BEFORE the question is asked.** Confirming
    /// a delete and then being told it was never allowed teaches the operator
    /// that the question is decoration, and the next one gets answered without
    /// reading it.
    pub(crate) fn begin_remove(&mut self) {
        self.status = None;
        let BackendSelection::Named(name) = &self.selected().selection else {
            self.status = Some("select a named backend to remove".into());
            return;
        };
        let name = name.clone();
        if let Some(refusal) = self.remove_refusal(&name) {
            self.status = Some(refusal);
            return;
        }
        self.mode = Mode::Confirm(Confirm {
            prompt: self.remove_prompt(&name),
            action: Pending::RemoveBackend(name),
        });
    }

    /// The question, naming what this particular deletion costs.
    ///
    /// A drop-in file and a `config.toml` entry are removed from different
    /// places, and an operator who is about to lose one should be told which.
    fn remove_prompt(&self, name: &str) -> String {
        let where_from = self
            .named_index(name)
            .map_or("configuration", |i| self.options[i].source.describe());
        format!("delete backend '{name}' from {where_from}? this cannot be undone  [y/N]")
    }

    /// Answer a pending confirmation. `y` performs it; anything else does not.
    ///
    /// **The default is no.** `[y/N]` is the same shape the vi `:wq` guard and
    /// the permission modal's `[d]eny (default)` already use, so a reflexive
    /// Enter or Esc is always the safe answer.
    fn answer_confirm(
        &mut self,
        yes: bool,
        remove: &mut dyn FnMut(&str) -> Result<String, String>,
    ) -> Option<bool> {
        let Mode::Confirm(confirm) = &self.mode else {
            return None;
        };
        let action = confirm.action.clone();
        self.mode = Mode::Choose;
        if !yes {
            self.status = Some("cancelled — nothing was deleted".to_string());
            return None;
        }
        match action {
            Pending::RemoveBackend(name) => self.remove_command(Some(&name), remove),
        }
    }

    pub(crate) fn in_confirm(&self) -> bool {
        matches!(self.mode, Mode::Confirm(_))
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
        if edit.replace && !edit.dirty.any() {
            // Nothing was touched: writing would only risk clobbering whatever
            // the file says now. Close the form as quietly as a no-op visit.
            self.status = Some(format!("no changes to backend '{}'", edit.name));
            self.mode = Mode::Choose;
            return false;
        }
        // Keep the row's provenance across an edit — a drop-in that also has an
        // inline entry stays marked as such.
        let source = self
            .named_index(&edit.name)
            .filter(|_| edit.replace)
            .map_or(BackendSource::UserDropIn, |i| self.options[i].source);
        match persist(&edit) {
            BackendSaveResult::Saved { note } => {
                let opt = BackendOption {
                    name: edit.name.clone(),
                    selection: BackendSelection::Named(edit.name.clone()),
                    source,
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

    /// Why this name cannot be removed right now, or `None` if it can.
    ///
    /// **One statement of the rules, asked twice.** `begin_remove` asks it to
    /// decide whether the question is worth posing; `remove_command` asks it
    /// again to enforce. A confirm that could still be refused would teach the
    /// operator that the question is decoration — and a confirm that BYPASSED
    /// the enforcement would be worse, so neither reads the other's answer.
    fn remove_refusal(&self, name: &str) -> Option<String> {
        let Some(idx) = self.named_index(name) else {
            return Some(format!("no configured backend named '{name}'"));
        };
        if !self.options[idx].editable() {
            return Some(self.options[idx].source.refusal(name));
        }
        // Two pointers may not be orphaned by a delete: the SESSION's active
        // backend, and config.toml's DURABLE `default_backend` — which diverges
        // from the active one whenever $NEWT_PROVIDER or a restored settings pin
        // names another backend, and whose dangling value is a hard
        // `UnknownNamed` error for `newt solve` / the ACP worker (review
        // §2/§7/§11). Either may only go together with a NEW selection applied in
        // the SAME transaction: spinner dialed to a different named backend →
        // close applying it, then the caller switches, repoints the default, and
        // deletes this file. Anything else is refused.
        let is_active = self.active == Some(idx);
        let is_default = self.default_backend.as_deref() == Some(name);
        if (is_active || is_default) && !self.picked_other_named(idx) {
            let role = match (is_active, is_default) {
                (true, true) => "the active backend and config.toml's default_backend",
                (true, false) => "the active backend",
                _ => "config.toml's default_backend",
            };
            return Some(format!(
                "'{name}' is {role} — dial another named backend first; then :d {name} \
                 switches and removes it in one transaction"
            ));
        }
        None
    }

    /// The spinner is deliberately parked on a DIFFERENT named backend — the
    /// precondition for removing the active one in a single transaction.
    fn picked_other_named(&self, idx: usize) -> bool {
        self.pick.is_dirty()
            && self.pick.value() != idx
            && matches!(
                self.options[self.pick.value()].selection,
                BackendSelection::Named(_)
            )
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
        if let Some(refusal) = self.remove_refusal(name) {
            self.status = Some(refusal);
            return None;
        }
        let Some(idx) = self.named_index(name) else {
            self.status = Some(format!("no configured backend named '{name}'"));
            return None;
        };
        if self.active == Some(idx) || self.default_backend.as_deref() == Some(name) {
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
            provenance: opt.source.provenance().to_string(),
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
                Field::Kind => ("kind", kind_label(form.kind()).to_string()),
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
    let dirty = form.dirty();
    // The URL is validated when the operator TYPED one (always, on an add). An
    // untouched url is not written back at all (the dirty-field overlay), so a
    // drop-in that legitimately has none — `kind = "embedded"`, which serves a
    // local model_path — stays editable instead of being held hostage to a URL
    // the form would then have to invent.
    if dirty.endpoint {
        if url.is_empty() {
            return Err("backend needs a url (e.g. http://host:11434)".to_string());
        }
        match reqwest::Url::parse(&url) {
            Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => {}
            _ => return Err(format!("invalid url '{url}' — needs http:// or https://")),
        }
    }
    let key_env = form.key_env.trim();
    if key_env.chars().any(|c| c.is_whitespace() || c == '=') {
        return Err("api-key env must be a bare variable name (e.g. OPENAI_API_KEY)".to_string());
    }
    Ok(BackendEdit {
        name,
        kind: form.kind(),
        endpoint: url,
        model: none_if_empty(&form.model),
        api_key_env: none_if_empty(&form.key_env),
        api_key_file: none_if_empty(&form.key_file),
        dirty,
        replace: form.editing.is_some(),
    })
}

/// The caller's summary line for a saved drop-in — pure so the caveat wording is
/// pinned by a test rather than buried in a closure. `also_inline` marks the
/// review §4 trap: a same-named inline `[[backends]]` entry keeps supplying the
/// `api_key_*` / `tiers` the drop-in omits, so CLEARING an auth field here does
/// not clear it in the resolved config.
pub(crate) fn saved_note(name: &str, path: &std::path::Path, also_inline: bool) -> String {
    let mut note = format!("saved backend '{name}' → {}", path.display());
    if also_inline {
        note.push_str(&format!(
            " (note: config.toml also declares [[backends]] '{name}' — fields this drop-in \
             omits, including a cleared api-key, are re-inherited from it; edit that entry too)"
        ));
    }
    note
}

/// Bordered block (2) + up to six rows + a hint/command/status row.
pub(crate) const PANEL_HEIGHT: u16 = 10;

fn draw(f: &mut ratatui::Frame, state: &PanelState) {
    let title = match &state.mode {
        Mode::Form(form) if form.editing.is_some() => " backend — edit ",
        Mode::Form(_) => " backend — add ",
        Mode::Confirm(_) => " backend — confirm delete ",
        _ => " backend — chooser ",
    };
    let bottom = if let Mode::Confirm(confirm) = &state.mode {
        // Deliberately the STATUS line, not the hint line: a question is not a
        // list of things you may do, and it should not be dressed as one.
        status_line(&confirm.prompt)
    } else if let Mode::Command(buf) = &state.mode {
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

/// A terminal I/O failure MID-PANEL, carrying the close contract for whatever
/// already committed to disk before it (review §5/§12). `changes` are file
/// operations that ALREADY happened, so losing them would leave the session
/// reporting nothing and running against a config it never re-resolved — the
/// exact invariant `PanelClose::changes` documents. The caller reports the error
/// AND honours the close.
#[derive(Debug)]
pub(crate) struct PanelRunError {
    pub error: io::Error,
    pub close: PanelClose,
}

impl From<io::Error> for PanelRunError {
    /// A failure before the loop owns any state (raw mode / terminal setup):
    /// nothing has been committed, so there is nothing to honour.
    fn from(error: io::Error) -> Self {
        Self {
            error,
            close: PanelClose::cancelled(),
        }
    }
}

/// The panel's exit, whichever way the loop ended: a mid-loop I/O error still
/// carries the committed file operations to the caller.
fn finish(
    loop_result: io::Result<()>,
    applied: bool,
    state: &PanelState,
) -> Result<PanelClose, PanelRunError> {
    match loop_result {
        Ok(()) => Ok(close_outcome(applied, state)),
        Err(error) => Err(PanelRunError {
            error,
            // Nothing was APPLIED (the loop never reached its exit), but the
            // add/edit/remove notes are real and already on disk.
            close: close_outcome(false, state),
        }),
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
    persist: impl FnMut(&BackendEdit) -> BackendSaveResult,
    remove: impl FnMut(&str) -> Result<String, String>,
    window: Option<crate::session_worker::PanelWindow>,
) -> Result<PanelClose, PanelRunError> {
    if seed.options.is_empty() {
        return Ok(PanelClose::cancelled());
    }
    let mut screen = BackendScreen {
        state: PanelState::new(seed),
        persist,
        remove,
    };
    // Under the cockpit the presenter lends this panel rows on the REAL
    // terminal; everywhere else the driver takes the bottom rows of stdout as
    // it always has. One `Option`, decided in one place.
    let driven = crate::panel::drive(&mut screen, PANEL_HEIGHT, window.as_ref());
    match driven {
        Ok(applied) => finish(Ok(()), applied, &screen.state),
        // Nothing was applied — the loop never reached its exit — but the
        // add/edit/remove notes are real and already on disk, so they are
        // still reported.
        Err(error) => finish(Err(error), false, &screen.state),
    }
}

/// The backend chooser as a [`crate::panel::Screen`]. Three key tables, one
/// per mode, exactly as the loop had them — what left is the terminal
/// lifecycle around them.
struct BackendScreen<P, R>
where
    P: FnMut(&BackendEdit) -> BackendSaveResult,
    R: FnMut(&str) -> Result<String, String>,
{
    state: PanelState,
    persist: P,
    remove: R,
}

impl<P, R> crate::panel::Screen for BackendScreen<P, R>
where
    P: FnMut(&BackendEdit) -> BackendSaveResult,
    R: FnMut(&str) -> Result<String, String>,
{
    fn draw(&self, frame: &mut ratatui::Frame) {
        draw(frame, &self.state);
    }

    fn key(&mut self, key: Key) -> crate::panel::Flow {
        use crate::panel::Flow;
        // Checked before every other mode: while a delete is pending, the
        // panel answers nothing else. A guard you can dismiss with `←` is not
        // a guard.
        if self.state.in_confirm() {
            match key {
                Key::Char('y') | Key::Char('Y') => {
                    if let Some(apply) = self.state.answer_confirm(true, &mut self.remove) {
                        return Flow::Close(apply);
                    }
                }
                // Everything else declines, including Enter. `[y/N]` means the
                // reflexive keystroke is the safe one.
                _ => {
                    self.state.answer_confirm(false, &mut self.remove);
                }
            }
            return Flow::Stay;
        }
        if self.state.in_command() {
            match key {
                Key::Char(c) => self.state.command_char(c),
                Key::Backspace => self.state.command_backspace(),
                Key::Esc => self.state.cancel_command(),
                Key::Enter => {
                    if let Some(apply) = self.state.run_command(&mut self.remove) {
                        return Flow::Close(apply);
                    }
                }
                _ => {}
            }
            return Flow::Stay;
        }
        if self.state.in_form() {
            match key {
                Key::Up => self.state.form_nav(-1),
                Key::Down => self.state.form_nav(1),
                Key::Left => self.state.form_cycle(-1),
                Key::Right => self.state.form_cycle(1),
                Key::Backspace => self.state.form_backspace(),
                Key::Enter => {
                    self.state.submit_form(&mut self.persist);
                }
                Key::Esc => self.state.cancel_form(),
                Key::Char(c) => self.state.form_input(c),
                _ => {}
            }
            return Flow::Stay;
        }
        match key {
            Key::Left => self.state.cycle(-1),
            Key::Right => self.state.cycle(1),
            // Plain by construction: Ctrl-E no longer opens the edit form.
            Key::Char('e') => self.state.begin_edit(),
            Key::Char('a') => self.state.begin_add(),
            Key::Char('d') => self.state.begin_remove(),
            Key::Char(':') => self.state.begin_command(""),
            Key::Enter => return Flow::Close(true),
            Key::Esc | Key::Char('q') => return Flow::Close(false),
            _ => {}
        }
        Flow::Stay
    }
}

#[cfg(test)]
#[path = "backend_panel_tests/mod.rs"]
mod backend_panel_tests;
