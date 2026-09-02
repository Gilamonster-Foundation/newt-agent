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

use crossterm::event::KeyCode;
use ratatui::text::Line;

use crate::config_panel::{clamp_step, hint_line, render_panel, status_line, RowView};
use crate::panel::{Flow, Screen};
use crate::settings_form::{Field, ValueSpace};

/// Bordered block (2) + one row per field + the backend door + a hint row.
///
/// Derived from the field count rather than a constant, so adding a setting
/// widens the panel instead of silently scrolling one off the bottom. When the
/// field list outgrows a short terminal the reservation clamps it (the
/// presenter gives what it can spare) — that is the point at which this grows
/// tabs, and not before: today's six rows fit, and paging them behind a tab
/// strip would be navigation cost for no gain.
pub(crate) fn panel_height() -> u16 {
    // Every field, plus the backend door.
    u16::try_from(Field::ALL.len() + 1)
        .unwrap_or(7)
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
        };
        Self {
            field,
            options,
            describe,
            value: current.clone(),
            opened_as: current,
            bounds,
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
    Door(DrillIn),
}

impl Row {
    fn label(&self) -> &'static str {
        match self {
            Self::Setting(row) => row.field.label(),
            Self::Door(door) => door.label,
        }
    }

    fn value(&self) -> String {
        match self {
            Self::Setting(row) => row.value.clone(),
            Self::Door(door) => door.value.clone(),
        }
    }
}

/// What the panel wants to happen after it closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Lines to print. Empty means the operator changed nothing.
    Applied(Vec<String>),
    /// Apply these, then open the backend chooser.
    ///
    /// The pick is NOT applied here. Choosing a backend reroutes the session,
    /// refreshes runtime state and reports from it — the caller's job, and
    /// already written once for `/backends`. A panel that did it again would
    /// be the second place that knows how a session switches backends.
    OpenBackends(Vec<String>),
}

pub(crate) struct SettingsPanel {
    rows: Vec<Row>,
    sel: usize,
    status: Option<String>,
    /// Set by Enter on a door; read by [`run`] once the panel closes.
    walk_through: bool,
}

impl SettingsPanel {
    /// `backend` is the session's current backend, for the door's value. `None`
    /// when there is nothing to show — the row still opens the chooser, which
    /// is exactly where an operator with no backend needs to go.
    pub(crate) fn new(backend: Option<String>) -> Self {
        let mut rows: Vec<Row> = Field::ALL
            .iter()
            .copied()
            .map(|field| Row::Setting(SettingRow::new(field)))
            .collect();
        rows.push(Row::Door(DrillIn {
            label: "backend",
            value: backend.unwrap_or_else(|| "(none)".to_string()),
            hint: "Enter: choose, edit, add or remove a backend",
        }));
        Self {
            rows,
            sel: 0,
            status: None,
            walk_through: false,
        }
    }

    fn settings(&self) -> impl Iterator<Item = &SettingRow> {
        self.rows.iter().filter_map(|row| match row {
            Row::Setting(row) => Some(row),
            Row::Door(_) => None,
        })
    }

    fn view_rows(&self) -> Vec<RowView> {
        self.rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let selected = i == self.sel;
                let (provenance, editable) = match row {
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
        let bottom: Line = match &self.status {
            Some(status) => status_line(status),
            None => hint_line("↑↓ select · ←→ change · Enter apply · Esc cancel"),
        };
        render_panel(frame, " settings ", &self.view_rows(), bottom, 26, 12);
    }

    fn key(&mut self, code: KeyCode, _ctrl: bool) -> Flow {
        match code {
            KeyCode::Up => self.sel = clamp_step(self.sel, -1, self.rows.len()),
            KeyCode::Down => self.sel = clamp_step(self.sel, 1, self.rows.len()),
            KeyCode::Left | KeyCode::Right => {
                let dir = if code == KeyCode::Left { -1 } else { 1 };
                if let Some(Row::Setting(row)) = self.rows.get_mut(self.sel) {
                    row.cycle(dir);
                }
                // The hint returns once a dial moves: a stale refusal beside a
                // value the operator has since changed reads as a live verdict
                // on the new one.
                self.status = None;
            }
            KeyCode::Enter => {
                // Enter on a door WALKS THROUGH it; Enter anywhere else applies
                // and closes. Both close this panel — the difference is what
                // the caller does next, which is why it is reported rather
                // than acted on here.
                self.walk_through = matches!(self.rows.get(self.sel), Some(Row::Door(_)));
                return Flow::Close(true);
            }
            KeyCode::Esc | KeyCode::Char('q') => return Flow::Close(false),
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
    window: Option<crate::session_worker::PanelWindow>,
) -> std::io::Result<Outcome> {
    let mut panel = SettingsPanel::new(backend);
    let applied = crate::panel::drive(&mut panel, panel_height(), window.as_ref())?;
    if !applied {
        return Ok(Outcome::Applied(vec!["settings: cancelled".to_string()]));
    }
    let messages = panel.commit();
    if panel.walk_through {
        // Pending dial changes are applied on the way through, not discarded:
        // the operator asked for both, and dropping half of it because they
        // left by a different door would be a surprise.
        return Ok(Outcome::OpenBackends(messages));
    }
    // An Enter that changed nothing is indistinguishable from Esc, which is
    // `/psyche`'s rule (#1665) for the same reason: a bare `/settings` opens
    // this panel, so browsing must never look like an edit.
    Ok(Outcome::Applied(if messages.is_empty() {
        vec!["settings: cancelled".to_string()]
    } else {
        messages
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::test_guard::GlobalSettingsGuard;

    fn panel() -> SettingsPanel {
        SettingsPanel::new(Some("sol".to_string()))
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
        panel.key(KeyCode::Up, false);
        assert_eq!(panel.sel, 0, "already at the top");
        for _ in 0..panel.rows.len() * 2 {
            panel.key(KeyCode::Down, false);
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
            ValueSpace::Number { .. } => panic!("edit-mode is a choice"),
        };
        // Walk right past the end; every value seen must be an offered one.
        for _ in 0..offered.len() + 2 {
            panel.key(KeyCode::Right, false);
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
            ValueSpace::Choice(_) => panic!("the round cap is a number"),
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
            panel.key(KeyCode::Right, false);
            if row_of(&panel, Field::EditMode).value == opened {
                panel.key(KeyCode::Left, false);
                panel.key(KeyCode::Left, false);
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
        assert_eq!(panel.key(KeyCode::Enter, false), Flow::Close(true));
        assert_eq!(panel.key(KeyCode::Esc, false), Flow::Close(false));
        assert_eq!(panel.key(KeyCode::Char('q'), false), Flow::Close(false));
        assert_eq!(panel.key(KeyCode::Char('x'), false), Flow::Stay);
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
            panel.key(KeyCode::Down, false);
        }
        let view = &panel.view_rows()[door];
        assert_eq!(view.value, "sol", "the door shows the current backend");
        assert!(!view.editable, "no `‹ ›` chrome: ←→ does not cycle a door");
        assert!(view.provenance.contains("Enter"), "{:?}", view.provenance);

        // ←→ on a door changes nothing, so a stray arrow cannot silently
        // repoint the backend.
        panel.key(KeyCode::Right, false);
        panel.key(KeyCode::Left, false);
        assert_eq!(panel.view_rows()[door].value, "sol");

        assert_eq!(panel.key(KeyCode::Enter, false), Flow::Close(true));
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
        assert_eq!(panel.key(KeyCode::Enter, false), Flow::Close(true));
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
        panel.key(KeyCode::Right, false);
        panel.key(KeyCode::Left, false);
        panel.key(KeyCode::Right, false);
        let dialled = row_of(&panel, Field::Thinking).value.clone();
        if dialled == opened {
            // A two-value dial that landed back where it started proves
            // nothing; step it once more.
            panel.key(KeyCode::Left, false);
        }
        assert_ne!(row_of(&panel, Field::Thinking).value, opened);

        panel.sel = door_index(&panel);
        panel.key(KeyCode::Enter, false);
        assert!(panel.walk_through);
        let applied = panel.commit();
        assert_eq!(applied.len(), 1, "the dialled row applied: {applied:?}");
        assert_eq!(
            Field::Thinking.current(),
            row_of(&panel, Field::Thinking).value,
            "and the setting actually moved"
        );
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
        panel.key(KeyCode::Right, false);
        panel.key(KeyCode::Down, false);
        let moved_off = &panel.view_rows()[0];
        assert!(
            moved_off.provenance.starts_with("was "),
            "a changed row shows what it was: {:?}",
            moved_off.provenance
        );
    }
}
