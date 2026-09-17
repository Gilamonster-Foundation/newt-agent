//! Permissions settings return operator intents; chat owns confirmation and persistence.

use crate::config_panel::{clamp_step, hint_line, render_panel, RowView};
use crate::list_cursor::ListCursor;
use crate::panel::{Flow, Key, Screen};
use newt_core::PermissionAction;

const VISIBLE: usize = 12;
const DEFAULTS: [PermissionAction; 2] = [PermissionAction::AllowOnce, PermissionAction::Deny];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionIntent {
    SetDefault(PermissionAction),
    SaveSessionAllows,
}

pub(crate) struct PermissionsPanel {
    lines: Vec<String>,
    default: PermissionAction,
    session_allows: usize,
    cursor: ListCursor,
    intent: Option<PermissionIntent>,
}

impl PermissionsPanel {
    pub(crate) fn new(
        lines: Vec<String>,
        default: PermissionAction,
        session_allows: usize,
    ) -> Self {
        let mut panel = Self {
            lines: Vec::new(),
            default: PermissionAction::Deny,
            session_allows: 0,
            cursor: ListCursor::new(2, VISIBLE, 0),
            intent: None,
        };
        panel.refresh(lines, default, session_allows);
        panel
    }

    pub(crate) fn refresh(
        &mut self,
        lines: Vec<String>,
        default: PermissionAction,
        session_allows: usize,
    ) {
        self.cursor = ListCursor::new(lines.len() + 2, VISIBLE, self.cursor.at());
        self.lines = lines;
        self.default = if DEFAULTS.contains(&default) {
            default
        } else {
            PermissionAction::Deny
        };
        self.session_allows = session_allows;
        self.intent = None;
    }

    fn take_intent(&mut self) -> Option<PermissionIntent> {
        self.intent.take()
    }

    fn rows(&self) -> Vec<RowView> {
        let mut rows = vec![
            RowView {
                label: "",
                value: format!(
                    "Default answer: {}",
                    if self.default == PermissionAction::AllowOnce {
                        "Allow once"
                    } else {
                        "Deny"
                    }
                ),
                provenance: "bare Enter at terminal permission prompts".to_string(),
                selected: self.cursor.at() == 0,
                editable: true,
            },
            RowView {
                label: "",
                value: "Make session allows permanent".to_string(),
                provenance: match self.session_allows {
                    0 => "No session allows to save".to_string(),
                    1 => "1 session allow · confirmation follows".to_string(),
                    count => format!("{count} session allows · confirmation follows"),
                },
                selected: self.cursor.at() == 1,
                editable: false,
            },
        ];
        rows.extend(self.lines.iter().enumerate().map(|(index, line)| RowView {
            label: "",
            value: line.clone(),
            provenance: String::new(),
            selected: self.cursor.at() == index + 2,
            editable: false,
        }));
        rows
    }
}

impl Screen for PermissionsPanel {
    fn draw(&self, frame: &mut ratatui::Frame) {
        render_panel(
            frame,
            "permissions",
            &self.rows(),
            hint_line("↑↓ select · ←→ default · Enter choose · Esc back"),
            0,
            38,
        );
    }

    fn key(&mut self, key: Key) -> Flow {
        match key {
            Key::Esc | Key::Ctrl('c') | Key::Ctrl('d') => {
                self.intent = None;
                return Flow::Close(false);
            }
            Key::Up | Key::Char('k') => self.cursor.step(-1),
            Key::Down | Key::Char('j') => self.cursor.step(1),
            Key::Ctrl('u') => self.cursor.step(-(self.cursor.page() as isize)),
            Key::Char('g') => self.cursor.home(),
            Key::Char('G') => self.cursor.end(),
            Key::Left | Key::Right if self.cursor.at() == 0 => {
                let current = usize::from(self.default == PermissionAction::Deny);
                let step = if key == Key::Left { -1 } else { 1 };
                self.default = DEFAULTS[clamp_step(current, step, DEFAULTS.len())];
            }
            Key::Enter => {
                self.intent = match self.cursor.at() {
                    0 => Some(PermissionIntent::SetDefault(self.default)),
                    1 if self.session_allows > 0 => Some(PermissionIntent::SaveSessionAllows),
                    _ => None,
                };
                if self.intent.is_some() {
                    return Flow::Close(true);
                }
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
    panel: &mut PermissionsPanel,
    window: Option<crate::session_worker::PanelWindow>,
) -> std::io::Result<Option<PermissionIntent>> {
    panel.intent = None;
    crate::panel::drive(panel, height(), window.as_ref())?;
    Ok(panel.take_intent())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::{Flow, Key, Screen};
    use newt_core::PermissionAction;

    fn panel(count: usize) -> PermissionsPanel {
        PermissionsPanel::new(
            vec![
                "posture: read_only".to_string(),
                "audit: no decisions".to_string(),
            ],
            PermissionAction::AllowOnce,
            count,
        )
    }

    #[test]
    fn default_dial_offers_only_allow_once_and_deny_and_requires_enter() {
        let mut panel = panel(2);
        assert!(panel.take_intent().is_none());
        assert_eq!(panel.default, PermissionAction::AllowOnce);
        assert_eq!(panel.key(Key::Left), Flow::Stay);
        assert_eq!(panel.default, PermissionAction::AllowOnce);
        assert_eq!(panel.key(Key::Right), Flow::Stay);
        assert_eq!(panel.default, PermissionAction::Deny);
        panel.key(Key::Right);
        assert_eq!(panel.default, PermissionAction::Deny);
        assert!(panel.take_intent().is_none());
        assert_eq!(panel.key(Key::Enter), Flow::Close(true));
        assert_eq!(
            panel.take_intent(),
            Some(PermissionIntent::SetDefault(PermissionAction::Deny))
        );
        assert!(
            panel.take_intent().is_none(),
            "an intent is consumed exactly once"
        );
    }

    #[test]
    fn cancel_and_control_exit_do_not_apply_a_draft() {
        for key in [Key::Esc, Key::Ctrl('c'), Key::Ctrl('d')] {
            let mut panel = panel(2);
            panel.key(Key::Right);
            assert_eq!(panel.key(key), Flow::Close(false));
            assert!(panel.take_intent().is_none());
        }
    }

    #[test]
    fn permanent_save_is_only_an_intent_and_requires_session_allows() {
        let mut empty = panel(0);
        empty.key(Key::Down);
        assert_eq!(empty.key(Key::Enter), Flow::Stay);
        assert!(empty.take_intent().is_none());
        let mut populated = panel(2);
        populated.key(Key::Down);
        assert_eq!(populated.key(Key::Enter), Flow::Close(true));
        assert_eq!(
            populated.take_intent(),
            Some(PermissionIntent::SaveSessionAllows)
        );
    }

    #[test]
    fn refresh_keeps_selection_and_discards_unapplied_default_changes() {
        let mut panel = panel(2);
        panel.key(Key::Right);
        panel.key(Key::Down);
        assert_eq!(panel.cursor.at(), 1);
        panel.refresh(
            vec!["posture: read_only".to_string()],
            PermissionAction::AllowOnce,
            1,
        );
        assert_eq!(panel.cursor.at(), 1);
        assert_eq!(panel.default, PermissionAction::AllowOnce);
        assert_eq!(panel.key(Key::Enter), Flow::Close(true));
        assert_eq!(
            panel.take_intent(),
            Some(PermissionIntent::SaveSessionAllows)
        );
    }

    #[test]
    fn status_rows_are_read_only_and_cursor_clamps() {
        let mut panel = panel(2);
        panel.key(Key::Up);
        assert_eq!(panel.cursor.at(), 0);
        for _ in 0..8 {
            panel.key(Key::Down);
        }
        assert_eq!(panel.cursor.at(), 3);
        assert_eq!(panel.key(Key::Enter), Flow::Stay);
        assert!(panel.take_intent().is_none());
    }

    #[test]
    fn unsupported_default_is_displayed_as_deny() {
        let panel = PermissionsPanel::new(Vec::new(), PermissionAction::AllowPermanent, 0);
        assert_eq!(panel.default, PermissionAction::Deny);
    }

    #[test]
    fn boxed_view_explains_default_save_count_and_empty_state() {
        for height in [9, 17] {
            for count in [0, 2] {
                let panel = panel(count);
                let mut term =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, height))
                        .unwrap();
                term.draw(|frame| panel.draw(frame)).unwrap();
                let buffer = term.backend().buffer();
                let text = buffer
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                for expected in [
                    "permissions",
                    "Default answer",
                    "Allow once",
                    "Make session allows permanent",
                    "posture: read_only",
                    "Esc back",
                ] {
                    assert!(
                        text.contains(expected),
                        "missing {expected:?} at height {height}: {text}"
                    );
                }
                assert!(text.contains(if count == 0 {
                    "No session allows to save"
                } else {
                    "2 session allows"
                }));
                assert_ne!(
                    buffer[(0, 0)].symbol(),
                    " ",
                    "shared boxed chrome is present"
                );
            }
        }
    }
}

// Model: GPT-6 | Harness: Codex | Operator: Shawn Hartsock | Time: 14:19 EDT | Date: 2026-09-16
