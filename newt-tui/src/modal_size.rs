//! **The height policy of a modal viewport** — grow, shrink, zoom — with no
//! terminal, no newt type, and no I/O: the host owns the terminal and the
//! layout, and asks this what height to request next.
//!
//! Written to lift into `newtui` unchanged (it depends on nothing), so every
//! modal — settings, a diff viewer, a Mermaid viewer, a Markdown pane — sizes
//! the same way under the same keys.

/// The operator's sizing intents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SizeKey {
    /// One row taller.
    Grow,
    /// One row shorter, never below [`MIN_ROWS`].
    Shrink,
    /// Fill the screen; again, back to the height before zooming.
    Zoom,
}

/// Border, one content row, the hint line, border: the least a modal can be
/// and still say how to leave.
pub(crate) const MIN_ROWS: u16 = 4;

/// Requested height as a fill request: the host clamps it to the screen.
pub(crate) const FILL: u16 = u16::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModalSize {
    /// The height to ask the host for. [`FILL`] while zoomed.
    requested: u16,
    /// The granted height to return to when zoom is toggled off.
    before_zoom: Option<u16>,
}

impl ModalSize {
    pub(crate) fn new(requested: u16) -> Self {
        Self {
            requested: requested.max(MIN_ROWS),
            before_zoom: None,
        }
    }

    pub(crate) fn requested(self) -> u16 {
        self.requested
    }

    #[cfg(test)]
    pub(crate) fn zoomed(self) -> bool {
        self.before_zoom.is_some()
    }

    /// Apply one key against the height the host actually `granted` (which
    /// the screen may have clamped). Returns the new request when it changed.
    ///
    /// Grow and Shrink step from what is ON SCREEN, not from the request, so
    /// holding Shift-↑ at full height does not bank rows the operator then
    /// has to shrink back through. Either one also leaves zoom.
    pub(crate) fn apply(&mut self, key: SizeKey, granted: u16) -> Option<u16> {
        let before = self.requested;
        match key {
            SizeKey::Grow => {
                self.before_zoom = None;
                self.requested = granted.saturating_add(1).max(MIN_ROWS);
            }
            SizeKey::Shrink => {
                self.before_zoom = None;
                self.requested = granted.saturating_sub(1).max(MIN_ROWS);
            }
            SizeKey::Zoom => match self.before_zoom.take() {
                Some(previous) => self.requested = previous,
                None => {
                    self.before_zoom = Some(granted.max(MIN_ROWS));
                    self.requested = FILL;
                }
            },
        }
        (self.requested != before).then_some(self.requested)
    }
}

#[cfg(test)]
#[path = "modal_size_tests.rs"]
mod tests;
