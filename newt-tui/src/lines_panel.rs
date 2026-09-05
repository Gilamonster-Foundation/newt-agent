//! **A read-only section: lines, with a scroll** (#2009 PR10a/PR13, #1979).
//!
//! Two sections want exactly this — Permissions (posture, prompted decisions,
//! the audit) and Audit (the receipts journal, loadout resolution, the
//! resolved config). They differ in their TITLE and their CONTENT and in
//! nothing else, so they are one panel with two callers rather than two panels
//! that will drift.
//!
//! Generalized at the second caller, not the fourth. `CLAUDE.md`'s reuse
//! discipline is written from a measured example — five spinner
//! implementations, four erase strategies — and the cheapest moment to not
//! repeat that is when the copy would be made.
//!
//! # Read-only, and that is what makes these hostable today
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

pub(crate) struct LinesPanel {
    title: &'static str,
    lines: Vec<String>,
    cursor: ListCursor,
}

impl LinesPanel {
    /// Build from lines the caller already produced.
    ///
    /// Every caller passes lines its own VERB already prints —
    /// `permissions_command_lines`, the receipts journal, the loadout
    /// resolution. **The panel renders the same words as the verb**: a second
    /// rendering of the same state is how a panel and its command come to
    /// disagree about what is true.
    pub(crate) fn new(title: &'static str, lines: Vec<String>) -> Self {
        let len = lines.len();
        Self {
            title,
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

impl Screen for LinesPanel {
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
            self.title,
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

/// The Audit section's rows: the settings-receipt journal, newest last.
///
/// # Every line says whether it still verifies
///
/// `SettingReceipt::is_intact` re-derives the address from the change and
/// compares it to the claim — *"a receipt whose `to` was edited after the fact
/// no longer computes to the id it carries"*. A viewer that printed the
/// receipts without that check would be a viewer that a tampered line reads
/// identically through, which is the one thing this record exists to prevent.
///
/// So the marker is per LINE, not a summary at the bottom: an intact journal
/// with one broken row must not read as "the journal is fine".
///
/// Pure over the file body, so the unit tier checks the rendering with no
/// filesystem — the fs read is the caller's.
pub(crate) fn receipt_audit_lines(body: &str) -> Vec<String> {
    let receipts = newt_core::settings_receipt::read_jsonl(body);
    if receipts.is_empty() {
        return vec!["no settings receipts yet".to_string()];
    }
    let mut out = Vec::with_capacity(receipts.len() + 1);
    let broken = receipts.iter().filter(|r| !r.is_intact()).count();
    out.push(match broken {
        0 => format!("{} receipts · all verify", receipts.len()),
        n => format!("{} receipts · {n} DO NOT VERIFY", receipts.len()),
    });
    for r in &receipts {
        // `✓`/`✗` per row, and the route (`via`) because the same change
        // through two verbs is two different events and which one was used is
        // the part a reader cannot reconstruct.
        let mark = if r.is_intact() { "✓" } else { "✗ TAMPERED" };
        out.push(format!(
            "{mark} {} · {} → {} · via {} · {}",
            r.change.setting,
            render_value(&r.change.from),
            render_value(&r.change.to),
            r.change.via,
            r.change.ts_claim,
        ));
    }
    out
}

/// A `SettingValue` as one short token for the audit row.
fn render_value(v: &newt_core::settings_receipt::SettingValue) -> String {
    serde_json::to_value(v)
        .ok()
        .map(|j| match j {
            // A token renders as itself; a structured value (the round cap's
            // derivation) renders as its JSON rather than being flattened to
            // a number that hides where it came from.
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        })
        .unwrap_or_else(|| "?".to_string())
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
        let mut panel = LinesPanel::new("t", lines(3));
        assert_eq!(panel.key(Key::Esc), Flow::Close(false));

        let mut panel = LinesPanel::new("t", lines(3));
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
        let mut panel = LinesPanel::new("t", lines(40));
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
        let mut panel = LinesPanel::new("t", lines(40));
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
        let mut panel = LinesPanel::new("t", Vec::new());
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

        let panel = LinesPanel::new(
            "permissions",
            vec![
                "active permission posture: strict — preset 'locked' floor: deny".to_string(),
                "prompted permissions: ON".to_string(),
            ],
        );
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

    /// **A tampered receipt is marked on its own line.**
    ///
    /// The whole point of addressing the record: a receipt whose `to` was
    /// edited no longer computes to the id it carries. A viewer that printed
    /// the rows without checking would read identically over a forged one.
    #[test]
    fn the_audit_marks_each_receipt_with_whether_it_still_verifies() {
        use newt_core::settings_receipt::{SettingChange, SettingReceipt, SettingValue};

        let change = SettingChange {
            schema: "newt.setting_change.v1".to_string(),
            setting: "thinking".to_string(),
            from: SettingValue::Token("off".to_string()),
            to: SettingValue::Token("fold".to_string()),
            via: "/settings".to_string(),
            ts_claim: "2026-09-05T10:00:00Z".to_string(),
        };
        let good = SettingReceipt::mint(change).expect("mint");
        let good_line = good.render_line().expect("render");

        // Same receipt with the `to` rewritten after minting: the id no longer
        // addresses the change it carries.
        let forged = good_line.replace("\"fold\"", "\"stream\"");
        assert_ne!(forged, good_line, "the fixture must actually differ");

        let rows = receipt_audit_lines(&format!("{good_line}\n{forged}"));
        assert!(rows[0].contains("2 receipts"), "{:?}", rows[0]);
        assert!(rows[0].contains("1 DO NOT VERIFY"), "{:?}", rows[0]);
        assert!(rows[1].starts_with('✓'), "{:?}", rows[1]);
        assert!(rows[2].starts_with('✗'), "{:?}", rows[2]);
        assert!(
            rows[1].contains("via /settings"),
            "the route is shown: {:?}",
            rows[1]
        );
    }

    /// An empty journal says so rather than rendering an empty panel — "no
    /// receipts yet" and "the viewer is broken" must not look the same.
    #[test]
    fn an_empty_journal_says_so() {
        assert_eq!(receipt_audit_lines(""), vec!["no settings receipts yet"]);
        assert_eq!(
            receipt_audit_lines("not json\n"),
            vec!["no settings receipts yet"],
            "unparseable lines are not receipts"
        );
    }

    /// A clean journal reports it, so an operator can tell "verified" from
    /// "not checked".
    #[test]
    fn a_clean_journal_reports_that_every_row_verifies() {
        use newt_core::settings_receipt::{SettingChange, SettingReceipt, SettingValue};
        let r = SettingReceipt::mint(SettingChange {
            schema: "newt.setting_change.v1".to_string(),
            setting: "markdown".to_string(),
            from: SettingValue::Token("auto".to_string()),
            to: SettingValue::Token("off".to_string()),
            via: "/markdown".to_string(),
            ts_claim: "2026-09-05T10:01:00Z".to_string(),
        })
        .expect("mint");
        let rows = receipt_audit_lines(&r.render_line().expect("render"));
        assert!(rows[0].contains("all verify"), "{:?}", rows[0]);
        assert!(rows[1].starts_with('✓'));
    }
}
