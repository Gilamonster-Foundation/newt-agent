//! **The model picker** — a windowed list you walk with the arrow keys.
//!
//! `/models` printed fifty-two names and asked you to type one back. The list
//! itself was fine; what was missing was any way to ACT on it, so the reply to
//! "which model?" was a second command and an exact spelling.
//!
//! # Why a list and not the dial that already existed
//!
//! `settings_panel` has a `ModelRow` — a `‹ prev / next ›` dial over the same
//! options. That shape is right for a vocabulary of three or four (edit-mode,
//! tenacity) and wrong for fifty-two: reaching the last entry is fifty-one
//! keypresses past a value you cannot see coming. A dial asks "which of these
//! few", a list asks "which of these many", and the served-model list is
//! emphatically the second question.
//!
//! The dial stays where it is. This is not a replacement for it; a settings
//! form with a fifty-row list embedded in it would be a worse settings form.
//!
//! # What is reused
//!
//! All of it. The loop is `panel::drive` (#2024), shared with `/psyche`,
//! `/settings` and `/backends`; the chrome is `config_panel::render_panel`,
//! which renders whatever slice it is handed; the cursor-and-window
//! arithmetic is `list_cursor`, which exists so the five panels behind it do
//! not each grow their own.
//!
//! This module is therefore only what is TRUE OF MODELS: which row is active,
//! that choosing the active one is a no-op, and what the rows say.

use crate::config_panel::{hint_line, render_panel, ModelChoice, RowView};
use crate::list_cursor::ListCursor;
use crate::panel::{Flow, Key, Screen};

/// What the operator did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Enter on a row: switch to this model.
    Chose(String),
    /// Esc, or Enter on the model already active.
    Cancelled,
}

pub(crate) struct ModelsPanel {
    models: Vec<ModelChoice>,
    active: String,
    cursor: ListCursor,
    chose: Option<String>,
}

/// Body rows the picker shows at once.
///
/// Nine, not "as many as fit": `panel::drive` takes a fixed inline height, and
/// a panel that claimed the whole terminal would push the conversation out of
/// view to answer a question about it. Nine is enough to see a neighbourhood
/// and short enough to leave the transcript legible behind it.
const VISIBLE: usize = 9;

/// Two border rows, the header, and the hint line, on top of the body.
pub(crate) fn panel_height() -> u16 {
    u16::try_from(VISIBLE + 4).unwrap_or(u16::MAX)
}

impl ModelsPanel {
    pub(crate) fn new(models: Vec<ModelChoice>, active: String) -> Self {
        // Open ON the active model rather than at the top. The question a
        // picker answers is "what else", and that is asked from where you are.
        let at = models
            .iter()
            .position(|m| m.name == active)
            .unwrap_or_default();
        Self {
            cursor: ListCursor::new(models.len(), VISIBLE, at),
            models,
            active,
            chose: None,
        }
    }

    /// The visible window, as rows the shared renderer understands.
    fn window(&self) -> Vec<RowView> {
        self.models
            .iter()
            .enumerate()
            .skip(self.cursor.top())
            .take(VISIBLE)
            .map(|(i, model)| RowView {
                label: "",
                value: model.name.clone(),
                // The active marker goes in the provenance column, which is
                // already the dim "where this came from" register.
                // The active marker rides the provenance column, which is
                // already the dim "where this came from" register. `tag` is
                // the cached conformance marker, shown but never acted on: an
                // untested model stays selectable, because refusing to dial
                // one would make the picker a gate rather than a chooser.
                provenance: if model.name == self.active {
                    format!("{}  ◂ active", model.tag)
                } else {
                    model.tag.clone()
                },
                selected: i == self.cursor.at(),
                editable: false,
            })
            .collect()
    }

    fn title(&self) -> String {
        if self.models.is_empty() {
            " models — none served ".to_string()
        } else {
            format!(
                " models — {} of {} ",
                self.cursor.at() + 1,
                self.models.len()
            )
        }
    }

    pub(crate) fn outcome(self) -> Outcome {
        match self.chose {
            // Enter on the model already running is a no-op, not a switch. It
            // would otherwise tear down and redial the session to arrive
            // exactly where it started.
            Some(name) if name != self.active => Outcome::Chose(name),
            _ => Outcome::Cancelled,
        }
    }
}

impl Screen for ModelsPanel {
    fn draw(&self, frame: &mut ratatui::Frame) {
        render_panel(
            frame,
            &self.title(),
            &self.window(),
            hint_line("↑↓/jk move · ^u/^d page · g/G ends · Enter choose · Esc cancel"),
            0,
            46,
        );
    }

    fn key(&mut self, key: Key) -> Flow {
        let page = self.cursor.page() as isize;
        match key {
            Key::Esc => Flow::Close(false),
            Key::Enter => {
                if let Some(model) = self.models.get(self.cursor.at()) {
                    self.chose = Some(model.name.clone());
                }
                Flow::Close(true)
            }
            // The vi vocabulary the rest of the crate already speaks —
            // `spill_view` and `transcript_pager` bind exactly these, so a
            // fourth scrolling surface should not invent a fifth set.
            Key::Up | Key::Char('k') => {
                self.cursor.step(-1);
                Flow::Stay
            }
            Key::Down | Key::Char('j') => {
                self.cursor.step(1);
                Flow::Stay
            }
            Key::Ctrl('u') => {
                self.cursor.step(-page);
                Flow::Stay
            }
            Key::Ctrl('d') => {
                self.cursor.step(page);
                Flow::Stay
            }
            Key::Char('g') => {
                self.cursor.home();
                Flow::Stay
            }
            Key::Char('G') => {
                self.cursor.end();
                Flow::Stay
            }
            _ => Flow::Stay,
        }
    }
}

/// Open the picker and report what the operator chose.
///
/// Assembled in ONE place, for `backend_chooser::choose`'s reason: a second
/// caller resolving the served list from config again would be a second answer
/// to "which models are choosable", and the two drift the first time one of
/// them learns something.
///
/// # Errors
///
/// The terminal could not be taken, built, polled, read or repainted.
pub(crate) fn choose(
    models: Vec<ModelChoice>,
    active: String,
    window: Option<crate::session_worker::PanelWindow>,
) -> std::io::Result<Outcome> {
    let mut panel = ModelsPanel::new(models, active);
    let applied = crate::panel::drive(&mut panel, panel_height(), window.as_ref())?;
    // A cancelled visit is silent — browse-and-leave costs nothing, the same
    // #1665 discipline the other panels keep.
    Ok(if applied {
        panel.outcome()
    } else {
        Outcome::Cancelled
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models(n: usize) -> Vec<ModelChoice> {
        (0..n)
            .map(|i| ModelChoice {
                name: format!("model-{i:02}"),
                tag: String::new(),
            })
            .collect()
    }

    fn panel(n: usize, active: &str) -> ModelsPanel {
        ModelsPanel::new(models(n), active.to_string())
    }

    /// The window invariant itself lives in `list_cursor`; what this asserts
    /// is that the picker's RENDERED rows track it — a correct cursor drawn
    /// through a wrong slice is still a picker you cannot use.
    #[test]
    fn the_rendered_window_always_contains_the_selected_row() {
        let mut p = panel(52, "model-00");
        for _ in 0..60 {
            p.key(Key::Down);
            assert_eq!(
                p.window().iter().filter(|r| r.selected).count(),
                1,
                "exactly one rendered row is selected"
            );
        }
        assert!(p
            .window()
            .iter()
            .any(|r| r.value == "model-51" && r.selected));
    }

    /// Opening ON the active model is the point: the question a picker answers
    /// is "what else", and that is asked from where you already are.
    #[test]
    fn it_opens_on_the_active_model_not_at_the_top() {
        let p = panel(52, "model-40");
        assert_eq!(p.cursor.at(), 40);
        assert!(
            p.window()
                .iter()
                .any(|r| r.value == "model-40" && r.selected),
            "the active model is visible AND selected on open"
        );
    }

    /// An active model the backend no longer serves must not silently point
    /// the cursor at a different one.
    #[test]
    fn an_unserved_active_model_opens_at_the_top_rather_than_guessing() {
        let p = panel(5, "a-model-that-went-away");
        assert_eq!(p.cursor.at(), 0);
    }

    /// Enter on the model already running is a no-op. Treating it as a switch
    /// would tear down and redial the session to arrive where it started.
    #[test]
    fn choosing_the_active_model_is_cancel_not_a_redial() {
        let mut p = panel(5, "model-02");
        assert_eq!(p.key(Key::Enter), Flow::Close(true));
        assert_eq!(p.outcome(), Outcome::Cancelled);

        let mut p = panel(5, "model-02");
        p.key(Key::Down);
        assert_eq!(p.key(Key::Enter), Flow::Close(true));
        assert_eq!(p.outcome(), Outcome::Chose("model-03".to_string()));
    }

    #[test]
    fn esc_cancels_without_choosing() {
        let mut p = panel(5, "model-00");
        p.key(Key::Down);
        assert_eq!(p.key(Key::Esc), Flow::Close(false));
        assert_eq!(p.outcome(), Outcome::Cancelled);
    }

    /// Paging and the ends, which is what makes fifty-two navigable at all.
    #[test]
    fn paging_and_the_ends_reach_the_whole_list() {
        let mut p = panel(52, "model-00");
        p.key(Key::Char('G'));
        assert_eq!(p.cursor.at(), 51);
        assert!(p.window().iter().any(|r| r.value == "model-51"));
        p.key(Key::Char('g'));
        assert_eq!(p.cursor.at(), 0);

        p.key(Key::Ctrl('d'));
        assert_eq!(
            p.cursor.at(),
            VISIBLE - 1,
            "a page is one window, less an overlap row"
        );
        p.key(Key::Ctrl('u'));
        assert_eq!(p.cursor.at(), 0);
    }

    /// A backend serving nothing must render and refuse, not panic. `/models`
    /// on an unreachable endpoint is a normal Tuesday.
    #[test]
    fn an_empty_list_is_survivable() {
        let mut p = ModelsPanel::new(Vec::new(), "whatever".to_string());
        assert_eq!(p.cursor.at(), 0);
        assert!(p.window().is_empty());
        assert!(p.title().contains("none served"));
        p.key(Key::Down);
        p.key(Key::Ctrl('d'));
        p.key(Key::Char('G'));
        assert_eq!(p.cursor.at(), 0);
        assert_eq!(p.key(Key::Enter), Flow::Close(true));
        assert_eq!(p.outcome(), Outcome::Cancelled, "nothing to choose");
    }

    /// A list shorter than the window must not scroll at all, or the last rows
    /// render against blank space the operator can walk into.
    #[test]
    fn a_short_list_never_scrolls() {
        let mut p = panel(3, "model-00");
        p.key(Key::Char('G'));
        assert_eq!(
            p.cursor.top(),
            0,
            "three rows in a nine-row window: no scroll"
        );
        assert_eq!(p.window().len(), 3);
    }

    /// The active marker rides the provenance column and marks exactly one row.
    #[test]
    fn exactly_one_row_is_marked_active() {
        let p = panel(52, "model-07");
        let marked: Vec<_> = p
            .window()
            .into_iter()
            .filter(|r| r.provenance.contains("active"))
            .collect();
        assert_eq!(marked.len(), 1);
        assert_eq!(marked[0].value, "model-07");
    }
}
