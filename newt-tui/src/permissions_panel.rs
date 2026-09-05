//! **The Permissions chooser** (#2009 PR10a, #1979).
//!
//! `/settings permissions` needs a surface before it can be a section, and the
//! register has said so for a while: the row is `Disposition::Panel`, the
//! word for *"a chooser that needs a usable region first (#1979)"*. The region
//! landed with `RegionLease` and the shell; this is the chooser.
//!
//! # Read-only, and that is what makes it hostable today
//!
//! §5.1's ledger says a section must LINK rather than host while its commit
//! path reads `run_chat` locals. **A read-only section has no commit path at
//! all**, so it sidesteps the precondition instead of waiting on it: this
//! panel renders lines it was handed and returns `Close(false)` on every exit.
//! Nothing it can do needs `active_posture` to move.
//!
//! What that defers, precisely, is the half that writes: the posture FIELD
//! (`handle_posture_command` preloads a skill body and applies a permission
//! clamp — not a value assignment), and grants / decision-reopen, which §4.4
//! parks for the event journal. Those arrive with #1999 at PR10c and the
//! ledger row moves then.
//!
//! # Composed, not invented
//!
//! Scroll arithmetic is [`ListCursor`], chrome is
//! [`crate::config_panel::render_panel`], the loop and the lease are
//! [`crate::panel::drive`]. `transcript_pager`'s `PagerState` was the obvious
//! candidate to reuse and is deliberately NOT reused: it is shaped for
//! conversation turns — prompt / reply / tool folds — and bending a
//! lines-with-a-scroll view through it would distort both.

use crate::list_cursor::ListCursor;
use crate::panel::{Flow, Key, Screen};

/// Rows visible at once. The panel leases this many plus chrome.
const VISIBLE: usize = 12;

pub(crate) struct PermissionsPanel {
    lines: Vec<String>,
    cursor: ListCursor,
}

impl PermissionsPanel {
    /// Build from lines the caller already produced.
    ///
    /// The caller passes `permissions_command_lines` and, when a log exists,
    /// `permission_audit_lines` — both pure, both already the text `/permissions`
    /// prints. **The panel renders the same words as the verb**: a second
    /// rendering of the same state is how a panel and its command come to
    /// disagree about what the posture is.
    pub(crate) fn new(lines: Vec<String>) -> Self {
        let len = lines.len();
        Self {
            lines,
            cursor: ListCursor::new(len, VISIBLE, 0),
        }
    }

    /// The height to lease: the window plus the chrome `render_panel` draws.
    pub(crate) fn height() -> u16 {
        u16::try_from(VISIBLE).unwrap_or(12).saturating_add(3)
    }

    #[cfg(test)]
    fn visible_rows(&self) -> Vec<&str> {
        self.lines
            .iter()
            .skip(self.cursor.top())
            .take(VISIBLE)
            .map(String::as_str)
            .collect()
    }
}

impl Screen for PermissionsPanel {
    fn draw(&self, frame: &mut ratatui::Frame) {
        let top = self.cursor.top();
        let rows: Vec<crate::config_panel::RowView> = self
            .lines
            .iter()
            .skip(top)
            .take(VISIBLE)
            .enumerate()
            .map(|(offset, line)| crate::config_panel::RowView {
                // The label column carries the whole line: these are sentences
                // and audit records, not `name: value` pairs, and splitting
                // them into columns would wrap them at a place they do not
                // mean anything.
                label: "",
                value: line.clone(),
                provenance: String::new(),
                selected: top + offset == self.cursor.at(),
                editable: false,
            })
            .collect();
        crate::config_panel::render_panel(
            frame,
            "permissions",
            &rows,
            crate::config_panel::hint_line("↑↓ scroll · ^u/^d page · Esc leave"),
            0,
            72,
        );
    }

    fn key(&mut self, key: Key) -> Flow {
        match key {
            // **Always `Close(false)`.** This panel reads; it never applies.
            // Reporting `true` would tell the shell an edit happened and put
            // "settings applied" in front of an operator who only looked.
            Key::Esc | Key::Enter => Flow::Close(false),
            Key::Up | Key::Char('k') => {
                self.cursor.step(-1);
                Flow::Stay
            }
            Key::Down | Key::Char('j') => {
                self.cursor.step(1);
                Flow::Stay
            }
            Key::Ctrl('u') => {
                let page = self.cursor.page() as isize;
                self.cursor.step(-page);
                Flow::Stay
            }
            Key::Ctrl('d') => {
                let page = self.cursor.page() as isize;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("row {i}")).collect()
    }

    /// **It reads; it never applies.** A panel that reported `true` would put
    /// "settings applied" in front of an operator who only looked at the audit.
    #[test]
    fn every_exit_reports_that_nothing_was_applied() {
        let mut panel = PermissionsPanel::new(lines(3));
        assert_eq!(panel.key(Key::Esc), Flow::Close(false));

        let mut panel = PermissionsPanel::new(lines(3));
        assert_eq!(
            panel.key(Key::Enter),
            Flow::Close(false),
            "Enter is not an edit here — there is nothing to edit"
        );
    }

    /// The window follows the cursor, and clamps rather than wrapping — a
    /// held `↓` on an audit log must not silently return to the top.
    #[test]
    fn scrolling_walks_the_lines_and_clamps_at_both_ends() {
        let mut panel = PermissionsPanel::new(lines(40));
        assert_eq!(panel.visible_rows().first().copied(), Some("row 0"));

        for _ in 0..60 {
            panel.key(Key::Down);
        }
        assert_eq!(
            panel.visible_rows().last().copied(),
            Some("row 39"),
            "clamped at the end, not wrapped"
        );

        for _ in 0..60 {
            panel.key(Key::Up);
        }
        assert_eq!(panel.visible_rows().first().copied(), Some("row 0"));
    }

    #[test]
    fn paging_and_the_ends_are_reachable_in_one_gesture() {
        let mut panel = PermissionsPanel::new(lines(40));
        panel.key(Key::Char('G'));
        assert_eq!(panel.visible_rows().last().copied(), Some("row 39"));
        panel.key(Key::Char('g'));
        assert_eq!(panel.visible_rows().first().copied(), Some("row 0"));

        // One page is a window less an overlap row, so the FIRST `^d` moves
        // the cursor to the last visible row without scrolling — the window
        // only follows once the cursor would leave it. Asserting on the
        // window after one page would be asserting the cursor is teleporting.
        panel.key(Key::Ctrl('d'));
        assert_eq!(
            panel.visible_rows().first().copied(),
            Some("row 0"),
            "the first page fills the window it already had"
        );
        panel.key(Key::Ctrl('d'));
        assert_ne!(
            panel.visible_rows().first().copied(),
            Some("row 0"),
            "the second page scrolls"
        );
        panel.key(Key::Ctrl('u'));
        panel.key(Key::Ctrl('u'));
        assert_eq!(panel.visible_rows().first().copied(), Some("row 0"));
    }

    /// A short log is inert rather than panicking — an empty permission log is
    /// a normal state, not an exceptional one.
    #[test]
    fn an_empty_or_short_view_is_survivable() {
        let mut panel = PermissionsPanel::new(Vec::new());
        for key in [Key::Down, Key::Up, Key::Ctrl('d'), Key::Char('G')] {
            assert_eq!(panel.key(key), Flow::Stay);
        }
        assert!(panel.visible_rows().is_empty());
        assert_eq!(panel.key(Key::Esc), Flow::Close(false));
    }

    /// The panel renders the lines it was handed, verbatim — it is the verb's
    /// own words, not a second rendering of the same state.
    #[test]
    fn it_renders_the_lines_it_was_given() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let panel = PermissionsPanel::new(vec![
            "active permission posture: strict — preset 'locked' floor: deny".to_string(),
            "prompted permissions: ON".to_string(),
        ]);
        let mut term = Terminal::new(TestBackend::new(90, 8)).unwrap();
        term.draw(|f| panel.draw(f)).unwrap();
        let buf = term.backend().buffer();
        let rendered: String = (0..8)
            .map(|y| {
                (0..90)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("permissions"), "{rendered}");
        assert!(rendered.contains("prompted permissions: ON"), "{rendered}");
        assert!(rendered.contains("Esc leave"), "{rendered}");
    }
}
