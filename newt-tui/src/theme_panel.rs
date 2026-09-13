//! Hosted theme editor. Draft styles are confined to preview rows until apply.
use crossterm::style::{Attribute, Color};
use newt_core::tty::theme::{active, parse_color, preferences, role_name, Theme, ALL_ROLES};
use ratatui::layout::{Constraint, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::config_panel::{clamp_step, hint_line, row_styles, status_line, RowView};
use crate::panel::{Flow, Key, Screen};

const COLORS: &[Color] = &[
    Color::Reset,
    Color::Black,
    Color::DarkGrey,
    Color::Grey,
    Color::White,
    Color::DarkRed,
    Color::Red,
    Color::DarkGreen,
    Color::Green,
    Color::DarkYellow,
    Color::Yellow,
    Color::DarkBlue,
    Color::Blue,
    Color::DarkMagenta,
    Color::Magenta,
    Color::DarkCyan,
    Color::Cyan,
];
const ATTRIBUTES: &[Attribute] = &[
    Attribute::Bold,
    Attribute::Dim,
    Attribute::Italic,
    Attribute::Underlined,
    Attribute::Reverse,
    Attribute::CrossedOut,
];
const LABELS: &[&str] = &[
    "Preset",
    "Role",
    "Color",
    "Bold",
    "Dim",
    "Italic",
    "Underline",
    "Reverse",
    "Strike",
    "Save name",
];

pub(crate) struct ThemePanel {
    themes: Vec<Theme>,
    draft: Theme,
    original: Theme,
    preset: usize,
    role: usize,
    selected: usize,
    name: String,
    status: String,
    color_input: Option<String>,
    pub(crate) applied: Option<String>,
}

impl ThemePanel {
    pub(crate) fn new() -> Self {
        let current = active().as_ref().clone();
        let (themes, status) = match preferences::list() {
            Ok((themes, warnings)) => (themes, warnings.join(" · ")),
            Err(error) => (preferences::builtins(), error),
        };
        Self::from_themes(current, themes, status)
    }

    fn from_themes(current: Theme, mut themes: Vec<Theme>, status: String) -> Self {
        if !themes.iter().any(|theme| theme.name == current.name) {
            themes.push(current.clone());
        }
        let preset = themes
            .iter()
            .position(|theme| theme.name == current.name)
            .unwrap_or(0);
        Self {
            themes,
            draft: current.clone(),
            original: current,
            preset,
            role: 0,
            selected: 0,
            name: String::new(),
            status,
            color_input: None,
            applied: None,
        }
    }

    fn cycle(&mut self, direction: i32) {
        match self.selected {
            0 => {
                self.preset = clamp_step(self.preset, direction, self.themes.len());
                self.draft = self.themes[self.preset].clone();
            }
            1 => self.role = clamp_step(self.role, direction, ALL_ROLES.len()),
            2 => {
                let role = ALL_ROLES[self.role];
                let mut style = self.draft.style(role);
                let at = COLORS
                    .iter()
                    .position(|color| Some(*color) == style.foreground_color);
                let next = at.map_or_else(
                    || if direction < 0 { COLORS.len() - 1 } else { 0 },
                    |at| clamp_step(at, direction, COLORS.len()),
                );
                style.foreground_color = Some(COLORS[next]);
                self.draft.set_style(role, style);
            }
            3..=8 => {
                let role = ALL_ROLES[self.role];
                let mut style = self.draft.style(role);
                style.attributes.toggle(ATTRIBUTES[self.selected - 3]);
                self.draft.set_style(role, style);
            }
            _ => {}
        }
        self.status.clear();
    }

    fn rows(&self) -> Vec<RowView> {
        let style = self.draft.style(ALL_ROLES[self.role]);
        let mut values = vec![
            self.draft.name.clone(),
            role_name(ALL_ROLES[self.role]).to_string(),
            self.color_input.as_ref().map_or_else(
                || preferences::color_name(style.foreground_color.unwrap_or(Color::Reset)),
                |value| format!("{value}▏"),
            ),
        ];
        values.extend(ATTRIBUTES.iter().map(|attribute| {
            if style.attributes.has(*attribute) {
                "on"
            } else {
                "off"
            }
            .to_string()
        }));
        values.push(if self.name.is_empty() {
            "type a new name".into()
        } else {
            self.name.clone()
        });
        let rows: Vec<_> = LABELS
            .iter()
            .zip(values)
            .enumerate()
            .map(|(index, (label, value))| RowView {
                label,
                value,
                provenance: String::new(),
                selected: index == self.selected,
                editable: index != 9,
            })
            .collect();
        rows
    }
}

impl Screen for ThemePanel {
    fn draw(&self, frame: &mut ratatui::Frame) {
        use newt_core::tty::theme::Role;
        let bottom: Line = if self.status.is_empty() {
            hint_line("↑↓ field · ←→ / type color · Ctrl-S save · Enter apply · Esc cancel")
        } else {
            status_line(&self.status)
        };
        let inner = crate::modal::frame(
            frame,
            frame.area(),
            &crate::modal::Chrome {
                title: "themes · draft preview",
                ..Default::default()
            },
        );
        let [form, preview, footer] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(5),
            Constraint::Length(1),
        ])
        .areas(inner);
        let rows = self.rows();
        let cursor = crate::list_cursor::ListCursor::new(
            rows.len(),
            usize::from(form.height),
            self.selected,
        );
        let lines: Vec<_> = rows
            .iter()
            .skip(cursor.top())
            .take(usize::from(form.height))
            .map(|row| {
                let (label_style, value_style) = row_styles(row.selected, row.editable);
                Line::from(vec![
                    Span::styled(
                        format!("{} {:10} ", if row.selected { "❯" } else { " " }, row.label),
                        label_style,
                    ),
                    Span::styled(
                        if row.selected && row.editable {
                            format!("‹ {} ›", row.value)
                        } else {
                            row.value.clone()
                        },
                        value_style,
                    ),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), form);
        let styled = |role, text: &'static str| {
            Span::styled(text, crate::theme::content_style(self.draft.style(role)))
        };
        let previews = vec![
            Line::from(vec![
                Span::raw("Preview: "),
                styled(ALL_ROLES[self.role], "The quick brown fox · 0123456789"),
            ]),
            Line::from(vec![
                styled(Role::MarkdownHeading, "Heading  "),
                styled(Role::InlineCode, "src/main.rs"),
            ]),
            Line::from(vec![
                Span::raw("Human: "),
                styled(Role::HumanText, "Please help with this task."),
            ]),
            Line::from(vec![
                Span::raw("Agent: "),
                styled(Role::AgentText, "Here is the result."),
            ]),
            Line::from(vec![
                Span::raw("Spill: "),
                styled(Role::Spill, "57 lines hidden · /spill open 20"),
            ]),
        ];
        frame.render_widget(Paragraph::new(previews), preview);
        frame.render_widget(Paragraph::new(bottom), footer);
    }

    fn key(&mut self, key: Key) -> Flow {
        if self.color_input.is_some()
            && matches!(key, Key::Up | Key::Down | Key::Enter | Key::Ctrl('s'))
        {
            let input = self.color_input.as_deref().unwrap_or_default();
            match parse_color(input) {
                Ok(color) => {
                    let role = ALL_ROLES[self.role];
                    let mut style = self.draft.style(role);
                    style.foreground_color = Some(color);
                    self.draft.set_style(role, style);
                    self.color_input = None;
                    self.status.clear();
                    if matches!(key, Key::Enter) {
                        return Flow::Stay;
                    }
                }
                Err(error) => {
                    self.status = error;
                    return Flow::Stay;
                }
            }
        }
        match key {
            Key::Char(character) if self.selected == 2 && !character.is_control() => {
                let input = self.color_input.get_or_insert_with(String::new);
                input.push(character);
                if let Ok(color) = parse_color(input) {
                    let role = ALL_ROLES[self.role];
                    let mut style = self.draft.style(role);
                    style.foreground_color = Some(color);
                    self.draft.set_style(role, style);
                }
                self.status = "Color: #rrggbb or 0–255 · Enter accepts · Esc cancels draft".into();
            }
            Key::Backspace if self.selected == 2 => {
                self.color_input.get_or_insert_with(String::new).pop();
            }
            Key::Up => self.selected = clamp_step(self.selected, -1, LABELS.len()),
            Key::Down => self.selected = clamp_step(self.selected, 1, LABELS.len()),
            Key::Left => {
                self.color_input = None;
                self.cycle(-1);
            }
            Key::Right => {
                self.color_input = None;
                self.cycle(1);
            }
            Key::Char(character) if self.selected == 9 && !character.is_control() => {
                self.name.push(character);
            }
            Key::Backspace if self.selected == 9 => {
                self.name.pop();
            }
            Key::Ctrl('s') => {
                let mut saved = self.draft.clone();
                saved.name = self.name.trim().to_string();
                match preferences::save(&saved) {
                    Ok(()) => {
                        self.draft = saved.clone();
                        if let Some(existing) = self
                            .themes
                            .iter_mut()
                            .find(|theme| theme.name == saved.name)
                        {
                            *existing = saved;
                        } else {
                            self.themes.push(saved);
                        }
                        self.preset = self
                            .themes
                            .iter()
                            .position(|theme| theme.name == self.draft.name)
                            .unwrap_or(0);
                        self.status = format!("saved {} · Enter applies", self.draft.name);
                    }
                    Err(error) => self.status = error,
                }
            }
            Key::Enter => match preferences::select(&self.draft) {
                Ok(()) => {
                    self.original = self.draft.clone();
                    self.applied = Some(format!("theme: {}", self.draft.name));
                    return Flow::Close(true);
                }
                Err(error) => self.status = error,
            },
            Key::Esc => {
                self.color_input = None;
                self.draft = self.original.clone();
                self.preset = self
                    .themes
                    .iter()
                    .position(|theme| theme.name == self.draft.name)
                    .unwrap_or(0);
                self.status.clear();
                return Flow::Close(false);
            }
            _ => {}
        }
        Flow::Stay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_and_cancelling_keep_the_original_style() {
        let original = Theme::builtin();
        let role = ALL_ROLES[0];
        let mut panel =
            ThemePanel::from_themes(original.clone(), vec![original.clone()], String::new());
        panel.selected = 3;
        panel.key(Key::Right);
        assert_ne!(panel.draft.style(role), original.style(role));
        assert_eq!(panel.original.style(role), original.style(role));
        assert!(matches!(panel.key(Key::Esc), Flow::Close(false)));
        assert_eq!(panel.draft.style(role), original.style(role));
    }

    #[test]
    fn role_edits_do_not_change_other_roles() {
        let original = Theme::builtin();
        let mut panel =
            ThemePanel::from_themes(original.clone(), vec![original.clone()], String::new());
        panel.selected = 5;
        panel.key(Key::Right);
        assert!(panel
            .draft
            .style(ALL_ROLES[0])
            .attributes
            .has(Attribute::Italic));
        assert_eq!(
            panel.draft.style(ALL_ROLES[1]),
            original.style(ALL_ROLES[1])
        );
        panel.key(Key::Left);
        assert_eq!(
            panel.draft.style(ALL_ROLES[0]),
            original.style(ALL_ROLES[0])
        );
    }

    #[test]
    fn preview_renders_the_draft_color_and_attributes() {
        let original = Theme::builtin();
        let mut panel = ThemePanel::from_themes(original.clone(), vec![original], String::new());
        let mut style = panel.draft.style(ALL_ROLES[0]);
        style.foreground_color = Some(Color::Magenta);
        style.attributes.set(Attribute::Italic);
        panel.draft.set_style(ALL_ROLES[0], style);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 17)).unwrap();
        terminal.draw(|frame| panel.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let cell = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "T" && cell.fg == ratatui::style::Color::LightMagenta)
            .expect("preview text uses the draft foreground");
        assert!(cell.modifier.contains(ratatui::style::Modifier::ITALIC));
        assert!(!cell.modifier.contains(ratatui::style::Modifier::DIM));
    }

    #[test]
    fn short_and_narrow_panels_keep_preview_visible_while_form_scrolls() {
        for height in [12, 17] {
            let original = Theme::builtin();
            let mut panel =
                ThemePanel::from_themes(original.clone(), vec![original], String::new());
            panel.selected = LABELS.len() - 1;
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, height)).unwrap();
            terminal.draw(|frame| panel.draw(frame)).unwrap();
            let screen: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            for expected in [
                "Save name",
                "Preview:",
                "Heading",
                "src/main.rs",
                "Human:",
                "Agent:",
                "Spill:",
            ] {
                assert!(
                    screen.contains(expected),
                    "{expected} missing at height {height}"
                );
            }
        }
    }

    #[test]
    fn explicit_colors_preview_without_applying_and_invalid_input_cannot_commit() {
        let original = Theme::builtin();
        let mut panel = ThemePanel::from_themes(original.clone(), vec![original], String::new());
        panel.selected = 2;
        for ch in "#123abc".chars() {
            panel.key(Key::Char(ch));
        }
        assert_eq!(
            panel.draft.color(ALL_ROLES[0]),
            Color::Rgb {
                r: 18,
                g: 58,
                b: 188
            }
        );
        assert!(matches!(panel.key(Key::Enter), Flow::Stay));
        assert!(panel.applied.is_none());
        for ch in "255".chars() {
            panel.key(Key::Char(ch));
        }
        assert!(matches!(panel.key(Key::Enter), Flow::Stay));
        assert_eq!(panel.draft.color(ALL_ROLES[0]), Color::AnsiValue(255));
        for ch in "256".chars() {
            panel.key(Key::Char(ch));
        }
        assert!(matches!(panel.key(Key::Enter), Flow::Stay));
        assert!(panel.applied.is_none());
        assert!(panel.color_input.is_some());
        assert!(!panel.status.is_empty());
    }

    #[test]
    fn name_entry_does_not_change_the_theme_or_apply_it() {
        let original = Theme::builtin();
        let mut panel =
            ThemePanel::from_themes(original.clone(), vec![original.clone()], String::new());
        panel.selected = 9;
        for ch in "my-theme".chars() {
            panel.key(Key::Char(ch));
        }
        assert_eq!(panel.name, "my-theme");
        assert_eq!(panel.draft.name, original.name);
        assert!(panel.applied.is_none());
        assert_eq!(panel.rows().len(), 10);
    }
}
