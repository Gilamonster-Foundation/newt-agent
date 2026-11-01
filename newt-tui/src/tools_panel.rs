//! `/settings tools` — the checklist of tools the model sees (#2932).
//!
//! The policy itself shipped in #2950 as `[tool_exposure] allow_list` and
//! `read_only`. This panel only EDITS it: a read-only toggle, a preset row
//! (`all` / `pi`), and one checkbox per tool in the catalog. It reports what
//! the operator chose and applies nothing — chat routes both values through
//! `settings_form::apply_and_record`, the one mutation path every other
//! setting takes, so the write is receipted and the loop re-resolves its
//! config from the same file the typed `/settings tools pi` writes.
//!
//! Composed, not invented: cursor arithmetic is [`ListCursor`], chrome is
//! [`render_panel`], the loop is [`crate::panel::drive`] — the same three
//! parts `permissions_panel` is made of.

use crate::config_panel::{hint_line, render_panel, RowView};
use crate::list_cursor::ListCursor;
use crate::panel::{Flow, Key, Screen};
use newt_core::ToolExposureConfig;

/// Rows above the first tool: the read-only toggle and the preset.
const FIXED_ROWS: usize = 2;

/// What the operator chose, as the `/settings` tokens the two fields take.
/// `None` for a value that did not move, so a browse-and-leave visit writes
/// nothing and journals nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ToolsChoice {
    pub(crate) tools: Option<String>,
    pub(crate) read_only: Option<&'static str>,
}

pub(crate) struct ToolsPanel {
    /// The catalog, plus any name an existing allow list carried that the
    /// catalog does not know (an MCP `server__tool`): shown, never dropped.
    names: Vec<String>,
    checked: Vec<bool>,
    /// `allow_list = None`: every tool, including ones not listed here.
    /// Distinct from every box ticked, which is an explicit list.
    all: bool,
    read_only: bool,
    opened_as: ToolExposureConfig,
    cursor: ListCursor,
    choice: Option<ToolsChoice>,
}

impl ToolsPanel {
    pub(crate) fn new(exposure: &ToolExposureConfig) -> Self {
        let mut names: Vec<String> = newt_core::agentic::builtin_tool_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        for extra in exposure.allow_list.iter().flatten() {
            if !names.contains(extra) {
                names.push(extra.clone());
            }
        }
        let checked = names
            .iter()
            .map(|name| {
                exposure
                    .allow_list
                    .as_ref()
                    .is_none_or(|list| list.contains(name))
            })
            .collect();
        Self {
            cursor: ListCursor::new(names.len() + FIXED_ROWS, VISIBLE, 0),
            names,
            checked,
            all: exposure.allow_list.is_none(),
            read_only: exposure.read_only,
            opened_as: exposure.clone(),
            choice: None,
        }
    }

    /// The policy as currently drafted.
    fn draft(&self) -> ToolExposureConfig {
        ToolExposureConfig {
            allow_list: (!self.all).then(|| {
                self.names
                    .iter()
                    .zip(&self.checked)
                    .filter(|(_, on)| **on)
                    .map(|(name, _)| name.clone())
                    .collect()
            }),
            read_only: self.read_only,
            ..self.opened_as.clone()
        }
    }

    fn preset_label(&self) -> String {
        let draft = self.draft();
        if self.all {
            "all".to_string()
        } else if draft.is_pi_preset() {
            "pi".to_string()
        } else {
            format!(
                "custom ({} of {})",
                self.checked.iter().filter(|on| **on).count(),
                self.names.len()
            )
        }
    }

    fn cycle_preset(&mut self) {
        if self.all {
            // → pi
            self.all = false;
            for (name, on) in self.names.iter().zip(self.checked.iter_mut()) {
                *on = newt_core::PI_PRESET_TOOLS.contains(&name.as_str());
            }
        } else {
            // → all
            self.all = true;
            self.checked.iter_mut().for_each(|on| *on = true);
        }
    }

    fn toggle_at(&mut self, row: usize) {
        match row {
            0 => self.read_only = !self.read_only,
            1 => self.cycle_preset(),
            i => {
                let i = i - FIXED_ROWS;
                // Unticking one box under `all` makes the list explicit.
                self.all = false;
                self.checked[i] = !self.checked[i];
            }
        }
    }

    /// What moved, as field tokens; `None` when nothing did.
    fn changes(&self) -> Option<ToolsChoice> {
        let draft = self.draft();
        let tools = (draft.allow_list != self.opened_as.allow_list)
            .then(|| crate::settings_form::tools_token(&draft));
        let read_only = (draft.read_only != self.opened_as.read_only)
            .then_some(if draft.read_only { "on" } else { "off" });
        (tools.is_some() || read_only.is_some()).then_some(ToolsChoice { tools, read_only })
    }

    fn take_choice(&mut self) -> Option<ToolsChoice> {
        self.choice.take()
    }

    fn rows(&self) -> Vec<RowView> {
        let at = self.cursor.at();
        let mut rows = vec![
            RowView {
                label: "",
                value: format!("read-only: {}", if self.read_only { "on" } else { "off" }),
                provenance: "hides write_file · edit_file · delete_file and clamps fs_write"
                    .to_string(),
                selected: at == 0,
                editable: true,
            },
            RowView {
                label: "",
                value: format!("preset: {}", self.preset_label()),
                provenance: "←→ all / pi · tick boxes below for a custom list".to_string(),
                selected: at == 1,
                editable: true,
            },
        ];
        rows.extend(
            self.names
                .iter()
                .zip(&self.checked)
                .enumerate()
                .map(|(i, (name, on))| RowView {
                    label: "",
                    value: format!("[{}] {name}", if *on { "x" } else { " " }),
                    provenance: if self.read_only && newt_core::agentic::read_only_hidden_tool(name)
                    {
                        "hidden by read-only".to_string()
                    } else {
                        String::new()
                    },
                    selected: at == i + FIXED_ROWS,
                    editable: true,
                }),
        );
        rows
    }
}

impl Screen for ToolsPanel {
    fn draw(&self, frame: &mut ratatui::Frame) {
        render_panel(
            frame,
            "tools",
            &self.rows(),
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
            Key::Ctrl('u') => self.cursor.step(-(self.cursor.page() as isize)),
            Key::Char('g') => self.cursor.home(),
            Key::Char('G') => self.cursor.end(),
            Key::Char(' ') | Key::Left | Key::Right => self.toggle_at(self.cursor.at()),
            Key::Enter => {
                // An Enter that changed nothing is indistinguishable from Esc
                // (`/psyche`'s rule, #1665): browsing must never look like an
                // edit.
                self.choice = self.changes();
                return Flow::Close(self.choice.is_some());
            }
            _ => {}
        }
        Flow::Stay
    }
}

/// Rows visible at once; the panel leases this many plus chrome.
const VISIBLE: usize = 12;

pub(crate) fn height() -> u16 {
    crate::lines_panel::LinesPanel::height()
}

pub(crate) fn run(
    panel: &mut ToolsPanel,
    window: Option<crate::session_worker::PanelWindow>,
) -> std::io::Result<Option<ToolsChoice>> {
    panel.choice = None;
    crate::panel::drive(panel, height(), window.as_ref())?;
    Ok(panel.take_choice())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> ToolsPanel {
        ToolsPanel::new(&ToolExposureConfig::default())
    }

    fn row_of(panel: &ToolsPanel, name: &str) -> usize {
        panel
            .names
            .iter()
            .position(|n| n == name)
            .expect("in the catalog")
            + FIXED_ROWS
    }

    fn go_to(panel: &mut ToolsPanel, row: usize) {
        panel.cursor.home();
        for _ in 0..row {
            panel.key(Key::Down);
        }
        assert_eq!(panel.cursor.at(), row);
    }

    /// Browsing is not editing: Enter with nothing moved closes like Esc and
    /// reports no choice; Esc after a change discards it.
    #[test]
    fn a_visit_that_changes_nothing_reports_nothing() {
        let mut panel = open();
        panel.key(Key::Down);
        panel.key(Key::Down);
        assert_eq!(panel.key(Key::Enter), Flow::Close(false));
        assert_eq!(panel.take_choice(), None);

        let mut panel = open();
        panel.key(Key::Char(' '));
        assert!(panel.read_only);
        assert_eq!(panel.key(Key::Esc), Flow::Close(false));
        assert_eq!(panel.take_choice(), None);
    }

    /// The preset row cycles all → pi → all, and the pi rendering is the
    /// token `/settings tools pi` takes, so the two doors write one value.
    #[test]
    fn the_preset_row_cycles_between_all_and_pi() {
        let mut panel = open();
        assert_eq!(panel.preset_label(), "all");
        go_to(&mut panel, 1);
        panel.key(Key::Right);
        assert_eq!(panel.preset_label(), "pi");
        assert!(panel.draft().is_pi_preset());
        assert_eq!(panel.key(Key::Enter), Flow::Close(true));
        assert_eq!(
            panel.take_choice(),
            Some(ToolsChoice {
                tools: Some("pi".to_string()),
                read_only: None,
            })
        );
        assert!(
            panel.take_choice().is_none(),
            "a choice is consumed exactly once"
        );

        let mut panel = ToolsPanel::new(&ToolExposureConfig {
            allow_list: Some(
                newt_core::PI_PRESET_TOOLS
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            ..Default::default()
        });
        assert_eq!(panel.preset_label(), "pi");
        go_to(&mut panel, 1);
        panel.key(Key::Left);
        assert_eq!(panel.preset_label(), "all");
        panel.key(Key::Enter);
        assert_eq!(panel.take_choice().unwrap().tools.as_deref(), Some("all"));
    }

    /// Unticking one box under `all` makes the list explicit; the token is
    /// the comma list the typed field takes; a second toggle restores it,
    /// and restoring every box is NOT `all` (an explicit list hides tools the
    /// catalog does not know).
    #[test]
    fn ticking_boxes_builds_an_explicit_list() {
        let mut panel = open();
        let git = row_of(&panel, "git");
        go_to(&mut panel, git);
        panel.key(Key::Char(' '));
        assert!(!panel.all);
        assert!(panel.preset_label().starts_with("custom ("));
        let token = panel.changes().unwrap().tools.unwrap();
        assert!(!token.split(',').any(|n| n == "git"), "{token}");
        assert!(token.split(',').any(|n| n == "read_file"), "{token}");

        panel.key(Key::Char(' '));
        assert!(panel.checked.iter().all(|on| *on));
        assert!(
            !panel.all,
            "every box ticked is an explicit list, not `all`"
        );
        assert!(panel.changes().is_some());
    }

    /// The read-only toggle is reported as its own field token, beside or
    /// without a tools change, and the write tools say why they are hidden.
    #[test]
    fn the_read_only_row_is_its_own_field() {
        let mut panel = open();
        panel.key(Key::Right);
        assert!(panel.read_only);
        let rows = panel.rows();
        assert!(rows[0].value.ends_with("on"));
        let write = row_of(&panel, "write_file");
        assert_eq!(rows[write].provenance, "hidden by read-only");
        assert_eq!(rows[row_of(&panel, "read_file")].provenance, "");
        assert_eq!(panel.key(Key::Enter), Flow::Close(true));
        assert_eq!(
            panel.take_choice(),
            Some(ToolsChoice {
                tools: None,
                read_only: Some("on"),
            })
        );
    }

    /// An allow list that names a tool the catalog does not know (an MCP
    /// `server__tool`) is shown and kept, never silently dropped on save.
    #[test]
    fn unknown_names_from_the_allow_list_are_kept() {
        let mut panel = ToolsPanel::new(&ToolExposureConfig {
            allow_list: Some(vec!["read_file".to_string(), "github__search".to_string()]),
            ..Default::default()
        });
        let mcp = row_of(&panel, "github__search");
        assert!(panel.checked[mcp - FIXED_ROWS]);
        go_to(&mut panel, 0);
        panel.key(Key::Char(' '));
        panel.key(Key::Enter);
        let choice = panel.take_choice().unwrap();
        assert_eq!(choice.tools, None, "the list did not move");
        assert_eq!(choice.read_only, Some("on"));
        let read = row_of(&panel, "read_file");
        go_to(&mut panel, read);
        panel.key(Key::Char(' '));
        assert_eq!(
            panel.changes().unwrap().tools.as_deref(),
            Some("github__search")
        );
    }
}
