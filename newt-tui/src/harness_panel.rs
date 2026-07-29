//! The harness config panel (#14) — the severable, TTY-only overlay fronting the
//! operator dials (`docs/decisions/harness_config_panel.md`, Accepted 2026-07-28):
//! the active model and the tenacity level.
//!
//! This module is the PURE state machine — fields, navigation, value cycling,
//! and the overlay text. Rendering to the terminal and key handling live in the
//! TUI shell; keeping the logic pure keeps it testable and keeps the
//! plain-scroller chat path untouched (the panel is a transient overlay the
//! operator opens and closes, never a widget in the chat flow). It writes through
//! the SAME setter (`set_cli_tenacity`) the flag and `/tenacity` use, so there is
//! one resolution order and no panel-only state.

use newt_core::Tenacity;

/// Which dial the cursor is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// The active model (display-only in v1; selection is a follow-up).
    Model,
    /// The tenacity level — cyclable.
    Tenacity,
}

const FIELDS: [Field; 2] = [Field::Model, Field::Tenacity];

/// The panel's state while it is open.
#[derive(Debug, Clone)]
pub struct HarnessPanel {
    model: String,
    tenacity: Tenacity,
    cursor: usize,
    /// Set once any dial is changed, so the shell knows to apply + redraw.
    dirty: bool,
}

impl HarnessPanel {
    /// Open the panel seeded with the session's current dials.
    pub fn new(model: impl Into<String>, tenacity: Tenacity) -> Self {
        Self {
            model: model.into(),
            tenacity,
            cursor: 0,
            dirty: false,
        }
    }

    pub fn selected(&self) -> Field {
        FIELDS[self.cursor]
    }

    pub fn tenacity(&self) -> Tenacity {
        self.tenacity
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Move the cursor to the next/previous dial (wrapping).
    pub fn select_next(&mut self) {
        self.cursor = (self.cursor + 1) % FIELDS.len();
    }
    pub fn select_prev(&mut self) {
        self.cursor = (self.cursor + FIELDS.len() - 1) % FIELDS.len();
    }

    /// Change the selected dial's value by `delta` (+1 raises tenacity, -1 lowers,
    /// wrapping). The model dial is read-only in v1. Returns whether anything
    /// changed. Does NOT touch process globals — the shell calls [`apply`] to
    /// commit through the shared setter.
    pub fn adjust(&mut self, delta: i32) -> bool {
        match self.selected() {
            Field::Tenacity => {
                let levels = Tenacity::all();
                let cur = levels.iter().position(|&t| t == self.tenacity).unwrap_or(1);
                let n = levels.len() as i32;
                let next = (((cur as i32) + delta).rem_euclid(n)) as usize;
                if levels[next] != self.tenacity {
                    self.tenacity = levels[next];
                    self.dirty = true;
                    return true;
                }
                false
            }
            Field::Model => false,
        }
    }

    /// Commit the panel's dials through the same process-global setters the flag
    /// and `/tenacity` use — the ONE write path. Called by the shell on change /
    /// close.
    pub fn apply(&self) {
        newt_core::tenacity::set_cli_tenacity(self.tenacity);
    }

    /// The overlay's content lines (the shell frames + positions them). The
    /// selected dial is marked with `›`; the tenacity line describes its effect.
    pub fn render_lines(&self) -> Vec<String> {
        let mark = |f: Field| if self.selected() == f { "›" } else { " " };
        vec![
            "harness — operator dials".to_string(),
            format!("{} model:    {}", mark(Field::Model), self.model),
            format!(
                "{} tenacity: {}  ({})",
                mark(Field::Tenacity),
                self.tenacity.label(),
                self.tenacity.describe()
            ),
            "↑↓ select · ←→ change · enter/esc close".to_string(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel() -> HarnessPanel {
        HarnessPanel::new("qwen3-coder_30b", Tenacity::Standard)
    }

    #[test]
    fn navigation_wraps_over_the_two_dials() {
        let mut p = panel();
        assert_eq!(p.selected(), Field::Model);
        p.select_next();
        assert_eq!(p.selected(), Field::Tenacity);
        p.select_next();
        assert_eq!(p.selected(), Field::Model, "wraps");
        p.select_prev();
        assert_eq!(p.selected(), Field::Tenacity, "wraps back");
    }

    #[test]
    fn adjust_cycles_tenacity_and_marks_dirty() {
        let mut p = panel();
        p.select_next(); // to Tenacity
        assert!(!p.dirty());
        assert!(p.adjust(1));
        assert_eq!(p.tenacity(), Tenacity::Insistent); // Standard → Insistent
        assert!(p.dirty());
        assert!(p.adjust(-1));
        assert_eq!(p.tenacity(), Tenacity::Standard);
        // Wrapping: from Relaxed, -1 lands on the most-forcing level.
        while p.tenacity() != Tenacity::Relaxed {
            p.adjust(-1);
        }
        p.adjust(-1);
        assert_eq!(p.tenacity(), Tenacity::Relentless, "wraps");
    }

    #[test]
    fn model_dial_is_read_only_in_v1() {
        let mut p = panel(); // cursor on Model
        assert!(!p.adjust(1));
        assert!(!p.adjust(-1));
        assert!(!p.dirty());
    }

    #[test]
    fn render_marks_the_selected_dial_and_shows_the_effect() {
        let mut p = panel();
        p.select_next(); // Tenacity
        let lines = p.render_lines();
        assert!(lines[0].contains("harness"));
        assert!(lines[1].starts_with(" model:") || lines[1].contains("model:"));
        // The tenacity line is marked and carries its description.
        let ten_line = lines.iter().find(|l| l.contains("tenacity:")).unwrap();
        assert!(ten_line.contains('›'), "selected dial marked: {ten_line}");
        assert!(
            ten_line.contains("read-only round"),
            "shows effect: {ten_line}"
        );
    }
}
