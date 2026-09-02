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

/// Bordered block (2) + one row per field + a hint/status row.
///
/// Derived from the field count rather than a constant, so adding a setting
/// widens the panel instead of silently scrolling one off the bottom. When the
/// field list outgrows a short terminal the reservation clamps it (the
/// presenter gives what it can spare) — that is the point at which this grows
/// tabs, and not before: today's six rows fit, and paging them behind a tab
/// strip would be navigation cost for no gain.
pub(crate) fn panel_height() -> u16 {
    u16::try_from(Field::ALL.len())
        .unwrap_or(6)
        .saturating_add(3)
}

/// One row: a field, and the value the operator has dialled to so far.
struct Row {
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

impl Row {
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

pub(crate) struct SettingsPanel {
    rows: Vec<Row>,
    sel: usize,
    status: Option<String>,
}

impl SettingsPanel {
    pub(crate) fn new() -> Self {
        Self {
            rows: Field::ALL.iter().copied().map(Row::new).collect(),
            sel: 0,
            status: None,
        }
    }

    fn view_rows(&self) -> Vec<RowView> {
        self.rows
            .iter()
            .enumerate()
            .map(|(i, row)| RowView {
                label: row.field.label(),
                value: row.value.clone(),
                provenance: if i == self.sel {
                    row.meaning()
                } else if row.is_dirty() {
                    format!("was {}", row.opened_as)
                } else {
                    String::new()
                },
                selected: i == self.sel,
                editable: true,
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
        self.rows
            .iter()
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
                if let Some(row) = self.rows.get_mut(self.sel) {
                    row.cycle(dir);
                }
                // The hint returns once a dial moves: a stale refusal beside a
                // value the operator has since changed reads as a live verdict
                // on the new one.
                self.status = None;
            }
            KeyCode::Enter => return Flow::Close(true),
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
    window: Option<crate::session_worker::PanelWindow>,
) -> std::io::Result<Vec<String>> {
    let mut panel = SettingsPanel::new();
    let applied = crate::panel::drive(&mut panel, panel_height(), window.as_ref())?;
    if !applied {
        return Ok(vec!["settings: cancelled".to_string()]);
    }
    let messages = panel.commit();
    // An Enter that changed nothing is indistinguishable from Esc, which is
    // `/psyche`'s rule (#1665) for the same reason: a bare `/settings` opens
    // this panel, so browsing must never look like an edit.
    Ok(if messages.is_empty() {
        vec!["settings: cancelled".to_string()]
    } else {
        messages
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::test_guard::GlobalSettingsGuard;

    fn panel() -> SettingsPanel {
        SettingsPanel::new()
    }

    fn row_of(panel: &SettingsPanel, field: Field) -> &Row {
        panel
            .rows
            .iter()
            .find(|r| r.field == field)
            .expect("the field has a row")
    }

    /// Every field the form offers gets a row, in the form's order. A panel
    /// that showed a subset would be a second, quieter answer to "what is
    /// settable".
    #[test]
    fn every_field_has_a_row() {
        let _g = GlobalSettingsGuard::acquire();
        let panel = panel();
        assert_eq!(panel.rows.len(), Field::ALL.len());
        for (row, field) in panel.rows.iter().zip(Field::ALL) {
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
        for _ in 0..Field::ALL.len() * 2 {
            panel.key(KeyCode::Down, false);
        }
        assert_eq!(panel.sel, Field::ALL.len() - 1, "stops at the bottom");
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
        let mut row = Row::new(Field::Rounds);

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
        assert!(panel.rows.iter().all(|r| !r.is_dirty()));
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
            let sel = panel
                .rows
                .iter()
                .position(|r| r.field == Field::EditMode)
                .expect("edit-mode has a row");
            panel.sel = sel;
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
    fn the_panel_is_sized_by_its_field_count() {
        let expected = u16::try_from(Field::ALL.len()).expect("few fields") + 3;
        assert_eq!(panel_height(), expected);
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
