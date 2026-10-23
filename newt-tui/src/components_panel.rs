//! `/settings system-prompt components` — the checklist of system prompt
//! components (#2933), the sibling of [`crate::tools_panel`].
//!
//! It only EDITS: one checkbox per component, with its size and what keeps it out.
//! The choice goes back to chat as `on <id>` / `off <id>` tokens, which take the
//! one mutation path every other setting takes. Toggling is presentation only: it
//! changes what the model is told, never what it may do.

use crate::config_panel::{hint_line, render_panel, RowView};
use crate::list_cursor::ListCursor;
use crate::panel::{Flow, Key, Screen};
use newt_core::prompt_components::{ComponentSet, Row};

/// Rows visible at once; the panel leases this many plus chrome.
const VISIBLE: usize = 12;

pub(crate) struct ComponentsPanel {
    rows: Vec<Row>,
    checked: Vec<bool>,
    opened_as: Vec<bool>,
    cursor: ListCursor,
    choice: Option<Vec<(&'static str, bool)>>,
}

impl ComponentsPanel {
    pub(crate) fn new(set: &ComponentSet, visible: &dyn Fn(&str) -> bool) -> Self {
        let rows = newt_core::prompt_components::rows(set, visible);
        let checked: Vec<bool> = rows.iter().map(|r| r.on).collect();
        Self {
            cursor: ListCursor::new(rows.len(), VISIBLE, 0),
            opened_as: checked.clone(),
            checked,
            rows,
            choice: None,
        }
    }

    /// What moved, as `(id, on)`; `None` when nothing did.
    fn changes(&self) -> Option<Vec<(&'static str, bool)>> {
        let moved: Vec<_> = self
            .rows
            .iter()
            .zip(self.checked.iter().zip(&self.opened_as))
            .filter(|(_, (now, was))| now != was)
            .map(|(row, (now, _))| (row.id, *now))
            .collect();
        (!moved.is_empty()).then_some(moved)
    }

    fn take_choice(&mut self) -> Option<Vec<(&'static str, bool)>> {
        self.choice.take()
    }

    fn rows_view(&self) -> Vec<RowView> {
        let at = self.cursor.at();
        self.rows
            .iter()
            .zip(&self.checked)
            .enumerate()
            .map(|(i, (row, on))| RowView {
                label: "",
                value: format!("[{}] {}", if *on { "x" } else { " " }, row.id),
                provenance: match row.needs {
                    Some(tool) if *on => format!("off (needs {tool}) · {}", row.description),
                    _ => format!(
                        "{} · {}{}",
                        crate::prompt_components_form::size_label(row.chars, row.layer),
                        row.description,
                        if row.fence {
                            crate::prompt_components_form::FENCE_NOTE
                        } else {
                            ""
                        }
                    ),
                },
                selected: at == i,
                editable: true,
            })
            .collect()
    }
}

impl Screen for ComponentsPanel {
    fn draw(&self, frame: &mut ratatui::Frame) {
        render_panel(
            frame,
            "system prompt components · presentation only",
            &self.rows_view(),
            hint_line("↑↓ select · Space/←→ toggle · Enter apply · Esc back"),
            0,
            48,
        );
    }

    fn key(&mut self, key: Key) -> Flow {
        match key {
            Key::Esc | Key::Char('q') | Key::Ctrl('c') | Key::Ctrl('d') => {
                self.choice = None;
                return Flow::Close(false);
            }
            Key::Up | Key::Char('k') => self.cursor.step(-1),
            Key::Down | Key::Char('j') => self.cursor.step(1),
            Key::Char('g') => self.cursor.home(),
            Key::Char('G') => self.cursor.end(),
            Key::Char(' ') | Key::Left | Key::Right => {
                let at = self.cursor.at();
                self.checked[at] = !self.checked[at];
            }
            Key::Enter => {
                // An Enter that changed nothing is an Esc: browsing is not editing.
                self.choice = self.changes();
                return Flow::Close(self.choice.is_some());
            }
            _ => {}
        }
        Flow::Stay
    }
}

pub(crate) fn height() -> u16 {
    crate::lines_panel::LinesPanel::height()
}

pub(crate) fn run(
    panel: &mut ComponentsPanel,
    window: Option<crate::session_worker::PanelWindow>,
) -> std::io::Result<Option<Vec<(&'static str, bool)>>> {
    panel.choice = None;
    crate::panel::drive(panel, height(), window.as_ref())?;
    Ok(panel.take_choice())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> ComponentsPanel {
        ComponentsPanel::new(&ComponentSet::default(), &|t| t != "render_report")
    }

    #[test]
    fn a_visit_that_changes_nothing_reports_nothing() {
        let mut panel = open();
        panel.key(Key::Down);
        assert_eq!(panel.key(Key::Enter), Flow::Close(false));
        assert_eq!(panel.take_choice(), None);
        let mut panel = open();
        panel.key(Key::Char(' '));
        assert_eq!(panel.key(Key::Esc), Flow::Close(false));
        assert_eq!(panel.take_choice(), None);
    }

    #[test]
    fn toggling_reports_exactly_what_moved_as_ids() {
        let mut panel = open();
        let at = panel.rows.iter().position(|r| r.id == "three-cs").unwrap();
        panel.cursor.home();
        for _ in 0..at {
            panel.key(Key::Down);
        }
        panel.key(Key::Char(' '));
        panel.cursor.home();
        panel.key(Key::Right);
        panel.key(Key::Right);
        assert_eq!(panel.key(Key::Enter), Flow::Close(true));
        assert_eq!(panel.take_choice(), Some(vec![("three-cs", false)]));
    }

    #[test]
    fn a_rule_whose_tool_is_hidden_says_what_it_needs() {
        let panel = open();
        let rows = panel.rows_view();
        let id = panel
            .rows
            .iter()
            .position(|r| r.id == "habit-present-findings")
            .unwrap();
        assert!(
            rows[id].provenance.contains("off (needs render_report)"),
            "{}",
            rows[id].provenance
        );
        assert!(rows[0].value.starts_with("[x] identity"));
    }
}
