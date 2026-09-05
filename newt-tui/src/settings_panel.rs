//! `/settings` as a PANEL — the knob space, navigated with arrows.
//!
//! The operator's ask, in their words: *"the /backends uses arrow navigation
//! and that's what I want for settings"*. `/settings` shipped as a sequence of
//! typed questions (#1981) because a chooser needs a usable region and there
//! was none — `slash_registry` still classifies this family as
//! `Disposition::Panel`, "a chooser that needs a region first". #1986 lent the
//! region and #2020 got it onto the real terminal under the cockpit, so the
//! region exists now and this is the chooser.
//!
//! # What this module is NOT allowed to be
//!
//! **A second settings vocabulary.** Every row here is a `settings_form::Field`
//! rendered through that module's own `value_space` / `current` / `label` /
//! `accepts` — widened to `pub(crate)` rather than copied. A panel that carried
//! its own list of what `tenacity` accepts would be a second answer to "what is
//! a valid setting", and the two would drift the first time a dial gained a
//! level.
//!
//! **A second mutation path.** Nothing here writes a setting. Enter hands each
//! changed field to `settings_form::apply_and_record`, which is the one place a
//! setting changes and the one place a receipt is written (#1965). The panel
//! decides WHICH changes to submit; it never performs one.
//!
//! **A third event loop.** The loop is `panel::drive` (#2024), shared with
//! `/psyche` and `/backends`. This module is a `Screen`: rows, a key table, and
//! what closing means.
//!
//! # Why the form stays
//!
//! The panel needs a TTY and the rich build. `newt solve` piped, the eval
//! harness, `newt-acp-worker` and newt-as-a-wyvern-worker all run off one, and
//! the plain-scroller rule (`docs/decisions/plain_scroller_tui.md`) is that
//! those paths keep working. So a bare `/settings` opens the panel where it
//! can and asks the questions where it cannot, and `/settings <field> <value>`
//! stays a deep link on every surface.

use crate::panel::Key;
use ratatui::text::Line;

use crate::config_panel::{clamp_step, hint_line, render_panel, ModelChoice, RowView};
use crate::panel::{Flow, Screen};
use crate::settings_form::{Field, ValueSpace};

/// Bordered block (2) + one row per field + the model dial + the backend door
/// + a hint row.
///
/// Derived from the field count rather than a constant, so adding a setting
/// widens the panel instead of silently scrolling one off the bottom. When the
/// field list outgrows a short terminal the reservation clamps it (the
/// presenter gives what it can spare) — that is the point at which this grows
/// tabs, and not before: today's six rows fit, and paging them behind a tab
/// strip would be navigation cost for no gain.
/// Index of the Session section in the shell — the one whose outcome
/// `commit()` belongs to.
const SESSION_SECTION: usize = 0;

pub(crate) fn panel_height() -> u16 {
    // Every field, plus the model dial and the backend door.
    u16::try_from(Field::ALL.len() + 2)
        .unwrap_or(8)
        .saturating_add(3)
}

/// A row that ENTERS another panel instead of holding a value.
///
/// The operator's ask — *"I want to slide to the /backends as one of the
/// settings I can choose"* — is a row, not a tab and not a nested pane. It
/// renders without the `‹ ›` dial chrome (`editable: false`), which is the
/// panel's existing way of saying "←→ does nothing here"; Enter opens the
/// chooser.
struct DrillIn {
    label: &'static str,
    /// What the setting is right now, so the row is worth reading before you
    /// walk through it.
    value: String,
    hint: &'static str,
}

/// The model row: a dial over what the active backend actually serves.
///
/// Its own kind rather than a `Field`, because a model is not a knob with a
/// closed vocabulary — the options are whatever the backend answered with when
/// the panel opened, and applying one is a NETWORK-VALIDATED switch that
/// `/model` already owns. Folding it into `settings_form` would drag a
/// ten-second blocking fetch into the fully-mocked unit tier, which the testing
/// strategy forbids for good reason.
struct ModelRow {
    /// What the backend serves, or `None` when it could not be reached. A
    /// `None` row renders and refuses to dial — #1666's rule, so an
    /// unreachable backend cannot silently look like "no models".
    options: Option<Vec<ModelChoice>>,
    at: usize,
    /// The model the session resolved when the panel opened.
    opened_as: String,
}

impl ModelRow {
    fn new(options: Option<Vec<ModelChoice>>, current: String) -> Self {
        // The ACTIVE model is always selectable, even when the served list
        // omits it (stale list, model just unloaded) — otherwise opening the
        // panel would silently reposition the dial and Enter could apply a
        // model nobody chose. The same guarantee `/psyche`'s spinner makes.
        let options = options.map(|mut list| {
            if !current.is_empty() && !list.iter().any(|m| m.name == current) {
                list.push(ModelChoice {
                    name: current.clone(),
                    tag: "(not served)".to_string(),
                });
            }
            list
        });
        let at = options
            .as_ref()
            .and_then(|list| list.iter().position(|m| m.name == current))
            .unwrap_or(0);
        Self {
            options,
            at,
            opened_as: current,
        }
    }

    fn name(&self) -> String {
        self.options
            .as_ref()
            .and_then(|list| list.get(self.at))
            .map_or_else(|| self.opened_as.clone(), |m| m.name.clone())
    }

    fn tag(&self) -> String {
        self.options
            .as_ref()
            .and_then(|list| list.get(self.at))
            .map_or_else(String::new, |m| m.tag.clone())
    }

    fn is_dirty(&self) -> bool {
        self.name() != self.opened_as
    }

    fn dialable(&self) -> bool {
        self.options.as_ref().is_some_and(|list| list.len() > 1)
    }

    fn cycle(&mut self, dir: i32) {
        if let Some(list) = self.options.as_ref().filter(|l| !l.is_empty()) {
            self.at = clamp_step(self.at, dir, list.len());
        }
    }
}

/// One row: a field, and the value the operator has dialled to so far.
struct SettingRow {
    field: Field,
    /// The vocabulary, when the field has one. Empty for a number.
    options: Vec<&'static str>,
    /// What each option MEANS, parallel to `options` — shown as the selected
    /// row's provenance column so the panel explains a dial without a second
    /// question, which is what the sequential form used a whole screen for.
    describe: Vec<String>,
    /// Index into `options`, or the number as typed, depending on the space.
    value: String,
    /// What the setting was when the panel opened. `value != opened_as` is the
    /// whole definition of dirty — and only dirty rows are submitted, so a
    /// browse-and-leave visit writes nothing and journals nothing.
    opened_as: String,
    bounds: Option<(usize, usize, &'static str)>,
    /// Set for a row that cannot be dialled. The panel is a dial surface and
    /// a template is not a vocabulary, so this row shows its value and names
    /// the door that edits it. Silently ignoring ←/→ would read as a broken
    /// row; the field editor that would make it dialable is PR8's.
    not_dialable: Option<String>,
}

impl SettingRow {
    fn new(field: Field) -> Self {
        let current = field.current();
        let (options, describe, bounds) = match field.value_space() {
            ValueSpace::Choice(offers) => (
                offers.iter().map(|(token, _)| *token).collect(),
                offers.iter().map(|(_, what)| what.clone()).collect(),
                None,
            ),
            ValueSpace::Number { release, min, max } => {
                (Vec::new(), Vec::new(), Some((min, max, release)))
            }
            ValueSpace::Text { .. } => (Vec::new(), Vec::new(), None),
        };
        let not_dialable = matches!(field.value_space(), ValueSpace::Text { .. })
            .then(|| format!("edit with /settings {} \"<template>\"", field.name()));
        Self {
            field,
            options,
            describe,
            value: current.clone(),
            opened_as: current,
            bounds,
            not_dialable,
        }
    }

    fn is_dirty(&self) -> bool {
        self.value != self.opened_as
    }

    /// Step the dial. A choice walks its vocabulary; a number walks its range
    /// with the release token (`auto`) sitting one step below the floor, so the
    /// operator can reach "let it derive again" with the same key rather than
    /// learning a second gesture.
    fn cycle(&mut self, dir: i32) {
        if let Some((min, max, release)) = self.bounds {
            let stepped = match self.value.parse::<usize>() {
                Ok(n) => {
                    let next = i64::from(n as u32) + i64::from(dir);
                    if next < min as i64 {
                        release.to_string()
                    } else {
                        next.clamp(min as i64, max as i64).to_string()
                    }
                }
                // On the release token: stepping up enters the range at its
                // floor, stepping down stays released.
                Err(_) if dir > 0 => min.to_string(),
                Err(_) => release.to_string(),
            };
            self.value = stepped;
            return;
        }
        if self.options.is_empty() {
            return;
        }
        let at = self
            .options
            .iter()
            .position(|o| *o == self.value)
            .unwrap_or(0);
        self.value = self.options[clamp_step(at, dir, self.options.len())].to_string();
    }

    /// What this row's current value means, for the provenance column.
    fn meaning(&self) -> String {
        if let Some(hint) = &self.not_dialable {
            return hint.clone();
        }
        self.options
            .iter()
            .position(|o| *o == self.value)
            .and_then(|i| self.describe.get(i).cloned())
            .unwrap_or_default()
    }
}

/// Either kind of row, in one list, because ↑↓ walks them together.
enum Row {
    Setting(SettingRow),
    Model(ModelRow),
    Door(DrillIn),
}

impl Row {
    fn label(&self) -> &'static str {
        match self {
            Self::Setting(row) => row.field.label(),
            Self::Model(_) => "model",
            Self::Door(door) => door.label,
        }
    }

    fn value(&self) -> String {
        match self {
            Self::Setting(row) => row.value.clone(),
            Self::Model(model) => model.name(),
            Self::Door(door) => door.value.clone(),
        }
    }
}

/// What the panel wants to happen after it closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Lines to print, and the model the operator dialled to (if any).
    ///
    /// The model pick is REPORTED, not applied. `/model` validates a name
    /// against what the backend actually serves, suggests a near-miss, and
    /// refuses an unserved one — a network round trip the caller already owns.
    /// A panel that applied it would be a second, unvalidated switch.
    Applied {
        lines: Vec<String>,
        model: Option<String>,
    },
    /// Apply these, then open the backend chooser. A model dialled on the way
    /// through is carried too, for the same reason a setting is.
    ///
    /// The pick is NOT applied here. Choosing a backend reroutes the session,
    /// refreshes runtime state and reports from it — the caller's job, and
    /// already written once for `/backends`. A panel that did it again would
    /// be the second place that knows how a session switches backends.
    OpenBackends {
        lines: Vec<String>,
        model: Option<String>,
    },
}

pub(crate) struct SettingsPanel {
    rows: Vec<Row>,
    sel: usize,
    /// Set by Enter on a door; read by [`run`] once the panel closes.
    walk_through: bool,
}

impl SettingsPanel {
    /// `backend` is the session's current backend, for the door's value. `None`
    /// when there is nothing to show — the row still opens the chooser, which
    /// is exactly where an operator with no backend needs to go.
    pub(crate) fn new(
        backend: Option<String>,
        models: Option<Vec<ModelChoice>>,
        current_model: String,
    ) -> Self {
        let mut rows: Vec<Row> = Field::ALL
            .iter()
            .copied()
            .map(|field| Row::Setting(SettingRow::new(field)))
            .collect();
        rows.push(Row::Model(ModelRow::new(models, current_model)));
        rows.push(Row::Door(DrillIn {
            label: "backend",
            value: backend.unwrap_or_else(|| "(none)".to_string()),
            hint: "Enter: choose, edit, add or remove a backend",
        }));
        Self {
            rows,
            sel: 0,
            walk_through: false,
        }
    }

    fn settings(&self) -> impl Iterator<Item = &SettingRow> {
        self.rows.iter().filter_map(|row| match row {
            Row::Setting(row) => Some(row),
            Row::Model(_) | Row::Door(_) => None,
        })
    }

    /// The model the operator dialled to, if they moved it.
    fn picked_model(&self) -> Option<String> {
        self.rows.iter().find_map(|row| match row {
            Row::Model(model) if model.is_dirty() => Some(model.name()),
            _ => None,
        })
    }

    fn view_rows(&self) -> Vec<RowView> {
        self.rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let selected = i == self.sel;
                let (provenance, editable) = match row {
                    Row::Model(model) => (
                        if selected && !model.dialable() {
                            // Says WHY it will not dial. A row that simply
                            // refused the arrow keys would read as broken.
                            match model.options {
                                None => "the active backend could not be listed".to_string(),
                                Some(_) => "the backend serves only this one".to_string(),
                            }
                        } else if selected {
                            model.tag()
                        } else if model.is_dirty() {
                            format!("was {}", model.opened_as)
                        } else {
                            model.tag()
                        },
                        model.dialable(),
                    ),
                    Row::Setting(setting) => (
                        if selected {
                            setting.meaning()
                        } else if setting.is_dirty() {
                            format!("was {}", setting.opened_as)
                        } else {
                            String::new()
                        },
                        // Dial chrome (`‹ value ›`) means ←→ moves this.
                        true,
                    ),
                    Row::Door(door) => (
                        if selected {
                            door.hint.to_string()
                        } else {
                            String::new()
                        },
                        // No chrome: a door is walked through, not dialled.
                        false,
                    ),
                };
                RowView {
                    label: row.label(),
                    value: row.value(),
                    provenance,
                    selected,
                    editable,
                }
            })
            .collect()
    }

    /// Submit every changed row, through the one mutation path.
    ///
    /// Returns the lines the caller prints. A refusal is reported and the rest
    /// still apply: the fields are independent settings, and abandoning four
    /// good changes because a fifth was malformed would be a transaction the
    /// operator never asked for.
    fn commit(&self) -> Vec<String> {
        self.settings()
            .filter(|row| row.is_dirty())
            .map(|row| {
                match crate::settings_form::apply_and_record(row.field, &row.value, "/settings") {
                    Ok(message) | Err(message) => message,
                }
            })
            .collect()
    }
}

impl Screen for SettingsPanel {
    fn draw(&self, frame: &mut ratatui::Frame) {
        // A hint, and nothing else. There is deliberately no status line: this
        // panel cannot fail while it is open. Every value it offers comes from
        // the field's own vocabulary, so a dial cannot land on something to
        // refuse, and the writes happen after it closes — a refusal there is
        // the CALLER's line to print, where the operator is already looking.
        //
        // The first cut had a `status: Option<String>` that nothing ever set,
        // which meant a `Some(..)` render arm that could not be reached. Found
        // by three independent designs of the isolation harness, all asking the
        // same question: which states can this component actually be in?
        let bottom: Line = hint_line("↑↓ select · ←→ change · Enter apply · Esc cancel");
        render_panel(frame, " settings ", &self.view_rows(), bottom, 26, 12);
    }

    fn key(&mut self, key: Key) -> Flow {
        match key {
            Key::Up => self.sel = clamp_step(self.sel, -1, self.rows.len()),
            Key::Down => self.sel = clamp_step(self.sel, 1, self.rows.len()),
            Key::Left | Key::Right => {
                let dir = if key == Key::Left { -1 } else { 1 };
                match self.rows.get_mut(self.sel) {
                    Some(Row::Setting(row)) => row.cycle(dir),
                    Some(Row::Model(row)) => row.cycle(dir),
                    // A door does not dial.
                    Some(Row::Door(_)) | None => {}
                }
            }
            Key::Enter => {
                // Enter on a door WALKS THROUGH it; Enter anywhere else applies
                // and closes. Both close this panel — the difference is what
                // the caller does next, which is why it is reported rather
                // than acted on here.
                self.walk_through = matches!(self.rows.get(self.sel), Some(Row::Door(_)));
                return Flow::Close(true);
            }
            // `Char` is plain by construction, so Ctrl-Q no longer cancels —
            // the flag used to be discarded here entirely.
            Key::Esc | Key::Char('q') => return Flow::Close(false),
            _ => {}
        }
        Flow::Stay
    }
}

/// Open the panel and report what changed.
///
/// `Some(lines)` when the panel ran; `None` when there is no region to draw in
/// (no window, and the terminal refused an inline one) so the caller falls back
/// to the typed form rather than printing nothing.
///
/// # Errors
///
/// The terminal could not be taken, built, polled, read or repainted.
pub(crate) fn run(
    backend: Option<String>,
    models: Option<Vec<ModelChoice>>,
    current_model: String,
    // The Permissions section's rows — `permissions_command_lines` plus the
    // audit tail, produced by the caller because they are the caller's state.
    // Passed as LINES rather than as the state itself, so this function never
    // learns what a posture is.
    permissions: Vec<String>,
    window: Option<crate::session_worker::PanelWindow>,
) -> std::io::Result<Outcome> {
    let mut panel = SettingsPanel::new(backend, models, current_model);
    // #2009 PR8/PR9: the panel is a SECTION of the shell rather than a panel
    // on its own, and the index now has a second row.
    //
    // **Backends is a LINK row** (§3.5 answer 3), not a hosted section. Its
    // commit path reads a dozen `run_chat` locals — cfg re-resolution, the
    // wire target, the pinned choice — so hosting it inside the shell would
    // mean relocating all of them (#1999) or duplicating that block. LINK mode
    // is the doc's declared fallback for exactly that: the index entry and the
    // receipts survive, only the single surface waits. Entering it closes the
    // shell with the SAME `OpenBackends` outcome the panel's own door row has
    // always produced, so the caller's path is unchanged.
    //
    // The panel is still owned HERE, so `commit()` and `picked_model()` below
    // read it exactly as before; the shell borrows it for the loop.
    // #2009 PR10b: Permissions is a HOSTED section, not a link. It is
    // read-only — status and the audit — so it has no commit path, and §5.1's
    // LINK rule is about a commit path that is out of reach. The half that
    // writes (the posture field, grants, decision reopen) waits for #1999.
    let mut permissions_panel = crate::permissions_panel::PermissionsPanel::new(permissions);
    let (applied, linked) = {
        let mut shell = crate::shell::Shell::new(vec![
            crate::shell::Section {
                name: "Session",
                accel: 's',
                summary: "dials, editor, reasoning, prompt".to_string(),
                body: crate::shell::Body::Screen(&mut panel),
            },
            crate::shell::Section {
                name: "Permissions",
                accel: 'p',
                summary: "posture · prompted decisions · audit".to_string(),
                body: crate::shell::Body::Screen(&mut permissions_panel),
            },
            crate::shell::Section {
                name: "Backends",
                accel: 'b',
                summary: "choose · edit · add · remove".to_string(),
                body: crate::shell::Body::Link,
            },
        ]);
        crate::panel::drive(&mut shell, panel_height(), window.as_ref())?;
        // **This section's flag, not the shell's.** With a second hosted
        // section the two are no longer the same question: `commit()` below
        // belongs to Session, and asking "did anything apply" would let a
        // future writing section put Session's messages on screen. Correct by
        // construction rather than by Permissions happening never to apply.
        (
            shell.section_applied(SESSION_SECTION),
            shell.linked().is_some(),
        )
    };
    if linked {
        // The index's Backends row and the panel's own door row are one act:
        // both leave through `OpenBackends`, so the caller opens the chooser
        // once, by one path.
        return Ok(Outcome::OpenBackends {
            lines: panel.commit(),
            model: panel.picked_model(),
        });
    }
    if !applied {
        return Ok(Outcome::Applied {
            lines: vec!["settings: cancelled".to_string()],
            model: None,
        });
    }
    let messages = panel.commit();
    let model = panel.picked_model();
    if panel.walk_through {
        // Pending dial changes are applied on the way through, not discarded:
        // the operator asked for both, and dropping half of it because they
        // left by a different door would be a surprise.
        return Ok(Outcome::OpenBackends {
            lines: messages,
            model,
        });
    }
    // An Enter that changed nothing is indistinguishable from Esc, which is
    // `/psyche`'s rule (#1665) for the same reason: a bare `/settings` opens
    // this panel, so browsing must never look like an edit. A model pick
    // counts as a change even though its message comes from the caller that
    // applies it.
    Ok(Outcome::Applied {
        lines: if messages.is_empty() && model.is_none() {
            vec!["settings: cancelled".to_string()]
        } else {
            messages
        },
        model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::test_guard::GlobalSettingsGuard;

    fn models(names: &[&str]) -> Option<Vec<ModelChoice>> {
        Some(
            names
                .iter()
                .map(|n| ModelChoice {
                    name: (*n).to_string(),
                    tag: String::new(),
                })
                .collect(),
        )
    }

    fn panel() -> SettingsPanel {
        SettingsPanel::new(
            Some("sol".to_string()),
            models(&["qwen3.5:397b", "nemotron:30b"]),
            "qwen3.5:397b".to_string(),
        )
    }

    fn model_index(panel: &SettingsPanel) -> usize {
        panel
            .rows
            .iter()
            .position(|r| matches!(r, Row::Model(_)))
            .expect("the model row exists")
    }

    fn model_row(panel: &SettingsPanel) -> &ModelRow {
        match &panel.rows[model_index(panel)] {
            Row::Model(row) => row,
            _ => unreachable!("model_index found it"),
        }
    }

    fn row_of(panel: &SettingsPanel, field: Field) -> &SettingRow {
        panel
            .settings()
            .find(|r| r.field == field)
            .expect("the field has a row")
    }

    fn index_of(panel: &SettingsPanel, field: Field) -> usize {
        panel
            .rows
            .iter()
            .position(|r| matches!(r, Row::Setting(row) if row.field == field))
            .expect("the field has a row")
    }

    /// The door is the LAST row, so ↓ from the bottom setting reaches it —
    /// which is what "slide to the backends" means with a keyboard.
    fn door_index(panel: &SettingsPanel) -> usize {
        panel
            .rows
            .iter()
            .position(|r| matches!(r, Row::Door(_)))
            .expect("the backend door exists")
    }

    /// Every field the form offers gets a row, in the form's order. A panel
    /// that showed a subset would be a second, quieter answer to "what is
    /// settable".
    #[test]
    fn every_field_has_a_row() {
        let _g = GlobalSettingsGuard::acquire();
        let panel = panel();
        assert_eq!(
            panel.settings().count(),
            Field::ALL.len(),
            "one row per field, plus the door"
        );
        for (row, field) in panel.settings().zip(Field::ALL) {
            assert_eq!(row.field, *field);
            assert_eq!(row.value, field.current(), "a row opens on the live value");
        }
    }

    /// ↑↓ move and CLAMP — they do not wrap. A wrapping cursor on a six-row
    /// panel puts the operator on `rounds` when they meant to stop at the top.
    #[test]
    fn selection_clamps_at_both_ends() {
        let _g = GlobalSettingsGuard::acquire();
        let mut panel = panel();
        panel.key(Key::Up);
        assert_eq!(panel.sel, 0, "already at the top");
        for _ in 0..panel.rows.len() * 2 {
            panel.key(Key::Down);
        }
        assert_eq!(panel.sel, panel.rows.len() - 1, "stops at the bottom");
    }

    /// ←→ walks a field's OWN vocabulary — the one `settings_form` publishes,
    /// not a list this module keeps.
    #[test]
    fn a_dial_walks_the_forms_vocabulary() {
        let _g = GlobalSettingsGuard::acquire();
        let mut panel = panel();
        let offered: Vec<&str> = match Field::EditMode.value_space() {
            ValueSpace::Choice(offers) => offers.iter().map(|(t, _)| *t).collect(),
            ValueSpace::Number { .. } | ValueSpace::Text { .. } => {
                panic!("edit-mode is a choice")
            }
        };
        // Walk right past the end; every value seen must be an offered one.
        for _ in 0..offered.len() + 2 {
            panel.key(Key::Right);
            let value = &row_of(&panel, Field::EditMode).value;
            assert!(offered.contains(&value.as_str()), "{value} is not offered");
        }
        assert_eq!(
            row_of(&panel, Field::EditMode).value,
            *offered.last().expect("a vocabulary"),
            "stepping past the end clamps rather than wrapping"
        );
    }

    /// The number row steps its range and reaches the release token by
    /// stepping BELOW the floor — one gesture for the whole space.
    #[test]
    fn the_number_row_steps_its_range_and_releases_below_the_floor() {
        let _g = GlobalSettingsGuard::acquire();
        let (min, max, release) = match Field::Rounds.value_space() {
            ValueSpace::Number { min, max, release } => (min, max, release),
            ValueSpace::Choice(_) | ValueSpace::Text { .. } => {
                panic!("the round cap is a number")
            }
        };
        let mut row = SettingRow::new(Field::Rounds);

        row.value = min.to_string();
        row.cycle(-1);
        assert_eq!(row.value, release, "below the floor is `auto`");
        row.cycle(-1);
        assert_eq!(row.value, release, "and stays there");
        row.cycle(1);
        assert_eq!(
            row.value,
            min.to_string(),
            "stepping up re-enters at the floor"
        );

        row.value = max.to_string();
        row.cycle(1);
        assert_eq!(row.value, max.to_string(), "the ceiling clamps");

        // Whatever the dial produces, the form must accept — this panel never
        // submits a value `/settings rounds` could refuse.
        for probe in [release.to_string(), min.to_string(), max.to_string()] {
            assert!(
                Field::Rounds.accepts(&probe).is_some(),
                "the dial produced {probe}, which the form refuses"
            );
        }
    }

    /// **Only changed rows are submitted.** A browse-and-leave visit must be
    /// indistinguishable from never opening the panel — no write, and no
    /// receipt claiming the operator set something to what it already was.
    #[test]
    fn an_untouched_panel_submits_nothing() {
        let _g = GlobalSettingsGuard::acquire();
        let panel = panel();
        assert!(panel.settings().all(|r| !r.is_dirty()));
        assert!(panel.commit().is_empty(), "nothing to apply");
    }

    /// A dialled row is dirty, applies through the recorded path, and the
    /// setting actually moves.
    #[test]
    fn a_dialled_row_applies_through_the_form() {
        let _g = GlobalSettingsGuard::acquire();
        let mut panel = panel();
        // Move the edit-mode dial off whatever it opened on.
        let opened = row_of(&panel, Field::EditMode).value.clone();
        while row_of(&panel, Field::EditMode).value == opened {
            panel.sel = index_of(&panel, Field::EditMode);
            panel.key(Key::Right);
            if row_of(&panel, Field::EditMode).value == opened {
                panel.key(Key::Left);
                panel.key(Key::Left);
            }
        }
        let dialled = row_of(&panel, Field::EditMode).value.clone();
        assert_ne!(dialled, opened);

        let messages = panel.commit();
        assert_eq!(messages.len(), 1, "only the dirty row: {messages:?}");
        assert!(messages[0].contains(&dialled), "{messages:?}");
        assert_eq!(
            Field::EditMode.current(),
            dialled,
            "the setting moved through the form"
        );
    }

    /// Enter closes as an apply, Esc as a cancel — and the key table says so
    /// without a terminal.
    #[test]
    fn enter_applies_and_escape_cancels() {
        let _g = GlobalSettingsGuard::acquire();
        let mut panel = panel();
        assert_eq!(panel.key(Key::Enter), Flow::Close(true));
        assert_eq!(panel.key(Key::Esc), Flow::Close(false));
        assert_eq!(panel.key(Key::Char('q')), Flow::Close(false));
        assert_eq!(panel.key(Key::Char('x')), Flow::Stay);
        // Ctrl-Q is not `q`. This panel used to discard the control flag
        // entirely, so it cancelled — and Ctrl-Q is flow control on some
        // terminals, which made it a cancel the operator never typed.
        assert_eq!(panel.key(Key::Ctrl('q')), Flow::Stay);
    }

    /// The panel is as tall as it has rows, so a new setting widens it rather
    /// than falling off the bottom.
    #[test]
    fn the_panel_is_sized_by_its_row_count() {
        let _g = GlobalSettingsGuard::acquire();
        let rows = u16::try_from(panel().rows.len()).expect("few rows");
        assert_eq!(panel_height(), rows + 3);
    }

    /// **The door is a row you slide to, and Enter walks through it.**
    ///
    /// The operator's ask in its own words: *"I want to slide to the /backends
    /// as one of the settings I can choose."* So it is the last row, reachable
    /// by ↓, and Enter on it reports `OpenBackends` rather than applying.
    #[test]
    fn the_backend_row_is_a_door_not_a_dial() {
        let _g = GlobalSettingsGuard::acquire();
        let mut panel = panel();
        let door = door_index(&panel);
        assert_eq!(door, panel.rows.len() - 1, "the door is the last row");

        // Slide to it.
        while panel.sel < door {
            panel.key(Key::Down);
        }
        let view = &panel.view_rows()[door];
        assert_eq!(view.value, "sol", "the door shows the current backend");
        assert!(!view.editable, "no `‹ ›` chrome: ←→ does not cycle a door");
        assert!(view.provenance.contains("Enter"), "{:?}", view.provenance);

        // ←→ on a door changes nothing, so a stray arrow cannot silently
        // repoint the backend.
        panel.key(Key::Right);
        panel.key(Key::Left);
        assert_eq!(panel.view_rows()[door].value, "sol");

        assert_eq!(panel.key(Key::Enter), Flow::Close(true));
        assert!(panel.walk_through, "Enter on the door walks through it");
    }

    /// Enter on a SETTING is an apply, not a walk-through — the flag is set
    /// per keypress, so arriving at Enter from a dial cannot open a panel the
    /// operator did not ask for.
    #[test]
    fn enter_on_a_setting_does_not_open_the_chooser() {
        let _g = GlobalSettingsGuard::acquire();
        let mut panel = panel();
        panel.sel = index_of(&panel, Field::Tenacity);
        assert_eq!(panel.key(Key::Enter), Flow::Close(true));
        assert!(!panel.walk_through);
    }

    /// Walking through the door still APPLIES what was dialled on the way.
    /// The operator asked for both; dropping half because they left by a
    /// different door would be a surprise.
    #[test]
    fn changes_made_before_the_door_are_applied_on_the_way_through() {
        let _g = GlobalSettingsGuard::acquire();
        let mut panel = panel();
        panel.sel = index_of(&panel, Field::Thinking);
        let opened = row_of(&panel, Field::Thinking).value.clone();
        panel.key(Key::Right);
        panel.key(Key::Left);
        panel.key(Key::Right);
        let dialled = row_of(&panel, Field::Thinking).value.clone();
        if dialled == opened {
            // A two-value dial that landed back where it started proves
            // nothing; step it once more.
            panel.key(Key::Left);
        }
        assert_ne!(row_of(&panel, Field::Thinking).value, opened);

        panel.sel = door_index(&panel);
        panel.key(Key::Enter);
        assert!(panel.walk_through);
        let applied = panel.commit();
        assert_eq!(applied.len(), 1, "the dialled row applied: {applied:?}");
        assert_eq!(
            Field::Thinking.current(),
            row_of(&panel, Field::Thinking).value,
            "and the setting actually moved"
        );
    }

    /// **←→ picks a model from what the backend actually serves.**
    ///
    /// The ask: *"when picking a model I want to use arrow keys to select the
    /// model."* The list is data the caller resolved before the panel opened —
    /// a fetch in a draw loop would freeze the terminal for as long as the
    /// backend took to answer.
    #[test]
    fn the_model_row_dials_the_served_list() {
        let _g = GlobalSettingsGuard::acquire();
        let mut panel = panel();
        panel.sel = model_index(&panel);
        assert_eq!(
            model_row(&panel).name(),
            "qwen3.5:397b",
            "opens on the active"
        );
        assert!(!model_row(&panel).is_dirty());
        assert_eq!(panel.picked_model(), None, "nothing picked yet");

        panel.key(Key::Right);
        assert_eq!(model_row(&panel).name(), "nemotron:30b");
        assert!(model_row(&panel).is_dirty());
        assert_eq!(panel.picked_model(), Some("nemotron:30b".to_string()));

        // Clamps rather than wrapping, like every other dial here.
        panel.key(Key::Right);
        assert_eq!(model_row(&panel).name(), "nemotron:30b");
        panel.key(Key::Left);
        panel.key(Key::Left);
        assert_eq!(model_row(&panel).name(), "qwen3.5:397b");
        assert!(!model_row(&panel).is_dirty(), "back where it started");
        assert_eq!(panel.picked_model(), None, "so nothing is picked");
    }

    /// **The ACTIVE model is always selectable**, even when the served list
    /// omits it — a stale list or a model just unloaded. Without the ghost the
    /// dial would silently reposition on open, and Enter would apply a model
    /// nobody chose. `/psyche`'s spinner makes the same guarantee (#1666).
    #[test]
    fn an_unserved_active_model_is_still_the_opening_position() {
        let row = ModelRow::new(models(&["a", "b"]), "gone-from-the-list".to_string());
        assert_eq!(
            row.name(),
            "gone-from-the-list",
            "opens on the active model"
        );
        assert!(!row.is_dirty(), "and that is not a change");
        assert_eq!(row.tag(), "(not served)", "said out loud, not hidden");
    }

    /// A backend that could not be listed renders the row and REFUSES to dial,
    /// saying why. A row that just ignored the arrows would read as broken.
    #[test]
    fn an_unlistable_backend_shows_the_row_and_will_not_dial() {
        let _g = GlobalSettingsGuard::acquire();
        let mut panel = SettingsPanel::new(Some("sol".into()), None, "qwen3.5:397b".into());
        panel.sel = model_index(&panel);
        assert_eq!(model_row(&panel).name(), "qwen3.5:397b", "still shown");
        assert!(!model_row(&panel).dialable());

        panel.key(Key::Right);
        assert_eq!(model_row(&panel).name(), "qwen3.5:397b", "unmoved");
        assert_eq!(panel.picked_model(), None);

        let view = &panel.view_rows()[model_index(&panel)];
        assert!(!view.editable, "no dial chrome on a row that cannot dial");
        assert!(
            view.provenance.contains("could not be listed"),
            "it says why: {:?}",
            view.provenance
        );

        // A single-model backend is the same shape: nothing to choose between.
        let one = SettingsPanel::new(Some("sol".into()), models(&["only"]), "only".into());
        assert!(!model_row(&one).dialable());
    }

    /// A model pick is REPORTED, never applied here — `/model` validates the
    /// name against what the backend serves and refuses an unserved one. The
    /// panel carrying that out itself would be a second, unvalidated switch.
    #[test]
    fn the_model_pick_is_reported_not_applied() {
        let _g = GlobalSettingsGuard::acquire();
        let mut panel = panel();
        panel.sel = model_index(&panel);
        panel.key(Key::Right);
        // `commit` is the SETTINGS writer; it must not have touched the model.
        assert!(
            panel.commit().is_empty(),
            "a model pick is not a settings write"
        );
        assert_eq!(panel.picked_model(), Some("nemotron:30b".to_string()));
    }

    /// The selected row explains itself, and a changed row says what it was —
    /// the two things the sequential form spent a whole question on.
    #[test]
    fn the_rows_carry_meaning_and_the_previous_value() {
        let _g = GlobalSettingsGuard::acquire();
        let mut panel = panel();
        let selected = &panel.view_rows()[0];
        assert!(
            !selected.provenance.is_empty(),
            "the selected row explains its value"
        );
        panel.key(Key::Right);
        panel.key(Key::Down);
        let moved_off = &panel.view_rows()[0];
        assert!(
            moved_off.provenance.starts_with("was "),
            "a changed row shows what it was: {:?}",
            moved_off.provenance
        );
    }
}
