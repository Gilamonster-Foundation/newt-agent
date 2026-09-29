//! **What the panel driver does with a key or a mouse event before any panel
//! sees it** — the meta prefix, resize mode, and a drag of the panel's top
//! border. Every panel gets these the same way because no panel implements
//! them.
//!
//! Pure: `drive` turns an [`Effect`] into terminal work; this decides it.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use newtui::Key;
use ratatui::layout::Rect;

use crate::modal_size::SizeKey;
use crate::prefix::{MetaAction, Sequencer, Step, BINDINGS};

/// What the driver should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Hand the key to the panel.
    Forward(Key),
    /// Size the modal.
    Size(SizeKey),
    /// Clear and repaint.
    Redraw,
    /// Consumed: nothing for the panel, nothing to do.
    Nothing,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Controls {
    meta: Sequencer,
    prefix: Key,
    resizing: bool,
    dragging: bool,
    help: bool,
}

impl Controls {
    pub(crate) fn new(prefix: Key) -> Self {
        Self {
            meta: Sequencer::new(prefix),
            prefix,
            resizing: false,
            dragging: false,
            help: false,
        }
    }

    pub(crate) fn key(&mut self, key: Key) -> Effect {
        if self.resizing {
            return match key {
                Key::Up => Effect::Size(SizeKey::Grow),
                Key::Down => Effect::Size(SizeKey::Shrink),
                Key::Enter | Key::Esc => {
                    self.resizing = false;
                    Effect::Nothing
                }
                _ => Effect::Nothing,
            };
        }
        self.help = false;
        match self.meta.feed(key, &BINDINGS) {
            Step::Pass(key) => Effect::Forward(key),
            Step::Armed | Step::Cancelled => Effect::Nothing,
            Step::Act(MetaAction::Zoom) => Effect::Size(SizeKey::Zoom),
            Step::Act(MetaAction::Redraw) => Effect::Redraw,
            Step::Act(MetaAction::Resize) => {
                self.resizing = true;
                Effect::Nothing
            }
            Step::Act(MetaAction::Help) => {
                self.help = true;
                Effect::Nothing
            }
        }
    }

    /// A left-button drag that starts on the panel's top border moves it:
    /// the new height runs from the pointer's row to the panel's bottom.
    pub(crate) fn mouse(&mut self, event: MouseEvent, area: Rect) -> Effect {
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if event.row == area.y => {
                self.dragging = true;
                Effect::Nothing
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                Effect::Size(SizeKey::To(area.bottom().saturating_sub(event.row)))
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.dragging = false;
                Effect::Nothing
            }
            _ => Effect::Nothing,
        }
    }

    /// The line drawn over the panel's hint while a mode is live, so the
    /// operator can see what the next key does.
    pub(crate) fn overlay(&self) -> Option<String> {
        if self.resizing {
            return Some("resize: ↑ grow · ↓ shrink · Enter or Esc done".to_string());
        }
        (self.meta.armed() || self.help).then(|| {
            format!(
                "{}: {}",
                crate::prefix::chord_label(self.prefix),
                BINDINGS.describe()
            )
        })
    }
}

#[cfg(test)]
#[path = "panel_controls_tests.rs"]
mod tests;
