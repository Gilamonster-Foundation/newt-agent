//! **The `/settings` shell — one parent loop, one lease** (#2009 PR8).
//!
//! The cut folds a lot of verbs into sections of `/settings`. Sections need a
//! surface, and the tempting way to build one is a new event loop that owns a
//! region and dispatches to panels. That is how a crate ends up with two loops,
//! two leases and two answers to "who owns the terminal right now" — the sprawl
//! `CLAUDE.md` measures in spinners and erase strategies.
//!
//! So this is **not a loop**. It is a [`Screen`] that happens to contain other
//! `Screen`s, driven by the same [`crate::panel::drive`] every panel already
//! uses. One loop, one `RegionLease`, one `PanelRawGuard`, one RAII restore on
//! every exit path including panic — not because this module is careful, but
//! because there is only one of each and this module did not write any of them.
//!
//! # What that buys, concretely
//!
//! - **`Shift` on open, and no resize at all.** §3.3 requires the lease be
//!   taken with `OnCollision::Shift` and that a resize use `relocate` with
//!   `Refuse`/`SuspendHolder`, never `Shift`. The shell leases ONE height —
//!   the tallest section — for its whole life, so the resize case it must get
//!   right does not arise. A rule you cannot break beats a rule you follow.
//! - **Sections stay pure.** A section is an existing `&mut dyn Screen`; the
//!   caller keeps the concrete panel and reads its outcome afterwards exactly
//!   as it does today. Nothing about how a panel commits changed.
//!
//! # Navigation
//!
//! The index lists sections with a one-line summary each. `↑↓`/`jk` move,
//! Enter or a section's accelerator enters it, `/` filters, Esc leaves — from
//! a section back to the index, from the index out of the shell.
//!
//! **Esc means "up one level", not "quit".** A section's own Esc already means
//! "leave this panel"; the shell reads that as "back to the index" rather than
//! closing the whole surface, so an operator who opened the wrong section is
//! one keypress from the right one instead of one keypress from the prompt.

use crate::list_cursor::ListCursor;
use crate::panel::{Flow, Key, Screen};

/// One entry in the index: a name, an accelerator, and the panel behind it.
///
/// Borrows the panel rather than owning it, so the caller keeps the concrete
/// type and reads its outcome — `SettingsPanel::commit`, a chooser's
/// selection — exactly as before the shell existed. A shell that owned its
/// sections would have to grow an outcome channel for each one, which is a
/// second way for a panel to report what it did.
pub(crate) struct Section<'a> {
    pub(crate) name: &'static str,
    pub(crate) accel: char,
    pub(crate) summary: String,
    pub(crate) body: Body<'a>,
}

/// What is behind a section row.
pub(crate) enum Body<'a> {
    /// A panel the shell drives in place.
    Screen(&'a mut dyn Screen),
    /// **LINK mode** (§3.5 answer 3): the row is in the index, but entering it
    /// CLOSES the shell and asks the caller to open the existing surface.
    ///
    /// The doc keeps this as "a live declared fallback", and §5.1 of
    /// `slash_radical_cut.md` is the running account of it being SPENT — which
    /// sections are linked, why, and what unblocks each. A slice that could
    /// host a section but has not relocated its state must take LINK mode and
    /// add a row there; silently duplicating a commit path to claim a section
    /// is the outcome that ledger exists to prevent.
    ///
    /// **The exit is one body of work, not a cleanup item.** Every linked
    /// section unblocks on #1999's relocation: moving the state its commit path
    /// reads out of `run_chat` and into core, the way `/markdown`, `/mode`,
    /// `compaction` and `detail` each moved ONE value. These paths are a dozen
    /// apiece. When it is done, this variant has no members left, and deleting
    /// it is the last commit of that work. The Backends commit path reads a dozen `run_chat`
    /// locals — cfg re-resolution, the wire target, the pinned choice — so
    /// hosting the panel inside the shell would mean either relocating all of
    /// them or duplicating the commit block. The index entry and the receipts
    /// survive; only the single surface waits.
    Link,
}

/// What the operator is looking at.
#[derive(Debug, Clone, PartialEq, Eq)]
enum View {
    /// The section list.
    Index,
    /// Typing a filter over the section list.
    Filtering(String),
    /// Inside a section, which owns every key until it closes.
    Section(usize),
}

pub(crate) struct Shell<'a> {
    sections: Vec<Section<'a>>,
    view: View,
    cursor: ListCursor,
    /// **Per section, not one flag for the shell** (#2009 PR9).
    ///
    /// Each panel's outcome is read by its own caller —
    /// `SettingsPanel::commit()`, `backend_panel::finish(.., applied, ..)` —
    /// and each needs to know whether ITS section applied. A single bool
    /// would tell the backend panel that a dial change in Session was its own,
    /// which is how a chooser reports a switch nobody asked for.
    ///
    /// Sticky per section: leaving through the index does not undo an edit.
    applied: Vec<bool>,
    /// The LINK row the operator chose, if any. Read after `drive` returns.
    linked: Option<usize>,
    /// **An index of one is not an index.**
    ///
    /// With a single section the shell opens straight into it and closes when
    /// it closes, so the surface is byte-for-byte what the panel was before
    /// the shell existed — no extra screen, no extra keypress.
    ///
    /// This is what lets the shell land BEFORE the sections that justify it.
    /// The alternative was shipping an index with one row for a release cycle,
    /// which is a worse `/settings` in exchange for scaffolding the operator
    /// did not ask for. The rule self-cancels the moment a second section
    /// arrives (PR9), and the test below is what will notice.
    pass_through: bool,
}

/// Rows the index shows at once. The lease is the tallest section, so this is
/// only a windowing bound for a very long section list.
const VISIBLE: usize = 9;

impl<'a> Shell<'a> {
    pub(crate) fn new(sections: Vec<Section<'a>>) -> Self {
        let len = sections.len();
        // A lone LINK row is not a pass-through: there is nothing to open
        // in place, and starting "inside" it would close the shell before the
        // operator saw anything.
        let pass_through =
            len == 1 && matches!(sections.first().map(|s| &s.body), Some(Body::Screen(_)));
        Self {
            sections,
            view: if pass_through {
                View::Section(0)
            } else {
                View::Index
            },
            cursor: ListCursor::new(len, VISIBLE, 0),
            applied: vec![false; len],
            linked: None,
            pass_through,
        }
    }

    /// Whether ANY section applied something — what `drive` returns.
    fn any_applied(&self) -> bool {
        self.applied.iter().any(|a| *a)
    }

    /// Whether the section at `index` applied something.
    ///
    /// **Live since #2009 PR10b**, which added the second hosted section.
    /// `settings_panel::run` asks whether SESSION applied, because `commit()`
    /// is Session's; "did anything apply" would let a future writing section
    /// put Session's messages on screen.
    ///
    /// It was `#[cfg(test)]` until a caller genuinely had two outcomes to tell
    /// apart — the ledger's third "done" marker, and it is the first of the
    /// three to fall.
    pub(crate) fn section_applied(&self, index: usize) -> bool {
        self.applied.get(index).copied().unwrap_or(false)
    }

    /// The indices matching the active filter, in index order.
    ///
    /// Substring, case-insensitive, over the section NAME — deliberately not
    /// fuzzy-across-everything. §3.2's fuzzy row filter is over the registry
    /// table INSIDE a section; at the index there are a handful of names and a
    /// substring is what an operator predicts.
    fn matches(&self) -> Vec<usize> {
        let needle = match &self.view {
            View::Filtering(f) => f.to_lowercase(),
            _ => String::new(),
        };
        (0..self.sections.len())
            .filter(|i| {
                needle.is_empty() || self.sections[*i].name.to_lowercase().contains(&needle)
            })
            .collect()
    }

    /// The section the index cursor is on, if any.
    fn selected(&self) -> Option<usize> {
        self.matches().get(self.cursor.at()).copied()
    }

    fn enter(&mut self, index: usize) -> Flow {
        // A LINK row does not open in place: it closes the shell and lets the
        // caller open the surface it names.
        if matches!(self.sections[index].body, Body::Link) {
            self.linked = Some(index);
            return Flow::Close(self.any_applied());
        }
        self.view = View::Section(index);
        Flow::Stay
    }

    /// The LINK row the operator chose, if any.
    pub(crate) fn linked(&self) -> Option<usize> {
        self.linked
    }

    /// Key handling for the index and the filter line.
    fn index_key(&mut self, key: Key) -> Flow {
        let filtering = matches!(self.view, View::Filtering(_));
        match key {
            Key::Esc => {
                if filtering {
                    // Esc clears the filter before it closes anything — the
                    // filter is a lens, and dropping the lens is the first
                    // thing "back" should mean.
                    self.view = View::Index;
                    self.cursor = ListCursor::new(self.sections.len(), VISIBLE, 0);
                    Flow::Stay
                } else {
                    Flow::Close(self.any_applied())
                }
            }
            Key::Up => {
                self.cursor.step(-1);
                Flow::Stay
            }
            Key::Down => {
                self.cursor.step(1);
                Flow::Stay
            }
            Key::Enter => match self.selected() {
                Some(index) => self.enter(index),
                // Enter on a filter that matches nothing does nothing rather
                // than closing: the operator is mid-search, not leaving.
                None => Flow::Stay,
            },
            Key::Backspace if filtering => {
                if let View::Filtering(f) = &mut self.view {
                    f.pop();
                    if f.is_empty() {
                        self.view = View::Index;
                    }
                }
                self.reveal_after_filter();
                Flow::Stay
            }
            Key::Char('/') if !filtering => {
                self.view = View::Filtering(String::new());
                self.reveal_after_filter();
                Flow::Stay
            }
            Key::Char(c) if filtering => {
                if let View::Filtering(f) = &mut self.view {
                    f.push(c);
                }
                self.reveal_after_filter();
                Flow::Stay
            }
            // vi movement and accelerators, only when NOT filtering — a filter
            // takes every character, including `j`, `k` and a section's own
            // accelerator, because those are letters someone is typing.
            Key::Char('j') => {
                self.cursor.step(1);
                Flow::Stay
            }
            Key::Char('k') => {
                self.cursor.step(-1);
                Flow::Stay
            }
            Key::Char(c) => match self
                .sections
                .iter()
                .position(|s| s.accel.eq_ignore_ascii_case(&c))
            {
                Some(index) => self.enter(index),
                None => Flow::Stay,
            },
            _ => Flow::Stay,
        }
    }

    /// Keep the cursor inside the filtered list after it changes length.
    fn reveal_after_filter(&mut self) {
        let len = self.matches().len();
        let at = self.cursor.at().min(len.saturating_sub(1));
        self.cursor = ListCursor::new(len, VISIBLE, at);
    }
}

impl Screen for Shell<'_> {
    fn draw(&self, frame: &mut ratatui::Frame) {
        if let View::Section(index) = self.view {
            // The section draws the WHOLE region: it is a panel that has
            // always drawn its own chrome, and re-framing it here would put a
            // border inside a border.
            if let Body::Screen(screen) = &self.sections[index].body {
                screen.draw(frame);
            }
            return;
        }

        let matches = self.matches();
        let top = self.cursor.top();
        let rows: Vec<crate::config_panel::RowView> = matches
            .iter()
            .skip(top)
            .take(VISIBLE)
            .enumerate()
            .map(|(offset, index)| {
                let section = &self.sections[*index];
                crate::config_panel::RowView {
                    label: section.name,
                    value: section.summary.clone(),
                    provenance: format!("[{}]", section.accel),
                    selected: top + offset == self.cursor.at(),
                    editable: true,
                }
            })
            .collect();

        let (title, hint) = match &self.view {
            View::Filtering(f) => (format!("settings — /{f}"), "Esc clears · Enter opens"),
            _ => (
                "settings".to_string(),
                "↑↓ move · Enter open · / filter · Esc leave",
            ),
        };
        crate::config_panel::render_panel(
            frame,
            &title,
            &rows,
            crate::config_panel::hint_line(hint),
            14,
            40,
        );
    }

    fn key(&mut self, key: Key) -> Flow {
        let View::Section(index) = self.view else {
            return self.index_key(key);
        };
        let Body::Screen(screen) = &mut self.sections[index].body else {
            // A LINK row is never the open view — `enter` closes instead.
            return Flow::Close(self.any_applied());
        };
        match screen.key(key) {
            Flow::Stay => Flow::Stay,
            // **A section closing returns to the INDEX, not out of the shell.**
            // The panel's own Esc means "leave this panel"; one level up is
            // where that lands. Whether it applied is remembered — leaving
            // through the index does not undo an edit.
            Flow::Close(applied) => {
                if let Some(slot) = self.applied.get_mut(index) {
                    *slot |= applied;
                }
                if self.pass_through {
                    return Flow::Close(self.any_applied());
                }
                self.view = View::Index;
                Flow::Stay
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A section stand-in that records what it was asked and answers on cue.
    ///
    /// The shell's contract is entirely about DELEGATION and the transitions
    /// around it, so the sections under test are doubles: no panel, no
    /// terminal, no state of their own. Fully mocked, per the unit tier.
    struct FakeSection {
        keys: RefCell<Vec<Key>>,
        answer: Flow,
        drawn: RefCell<usize>,
    }

    impl FakeSection {
        fn new(answer: Flow) -> Self {
            Self {
                keys: RefCell::new(Vec::new()),
                answer,
                drawn: RefCell::new(0),
            }
        }
    }

    impl Screen for FakeSection {
        fn draw(&self, _f: &mut ratatui::Frame) {
            *self.drawn.borrow_mut() += 1;
        }
        fn key(&mut self, key: Key) -> Flow {
            self.keys.borrow_mut().push(key);
            self.answer
        }
    }

    fn section<'a>(name: &'static str, accel: char, screen: &'a mut dyn Screen) -> Section<'a> {
        Section {
            name,
            accel,
            summary: String::new(),
            body: Body::Screen(screen),
        }
    }

    fn link<'a>(name: &'static str, accel: char) -> Section<'a> {
        Section {
            name,
            accel,
            summary: String::new(),
            body: Body::Link,
        }
    }

    #[test]
    fn enter_opens_the_selected_section_and_esc_returns_to_the_index() {
        let mut a = FakeSection::new(Flow::Stay);
        let mut b = FakeSection::new(Flow::Close(false));
        let mut shell = Shell::new(vec![
            section("Session", 's', &mut a),
            section("Backends", 'b', &mut b),
        ]);

        assert_eq!(shell.view, View::Index);
        shell.key(Key::Down);
        assert_eq!(shell.key(Key::Enter), Flow::Stay);
        assert_eq!(shell.view, View::Section(1), "Enter opens the cursor's row");

        // The section answers Close; the shell reads that as "up one level".
        assert_eq!(shell.key(Key::Esc), Flow::Stay);
        assert_eq!(shell.view, View::Index, "a closing section returns here");
    }

    /// **Esc from the index leaves; Esc from a section does not** — with more
    /// than one section. The whole reason the shell interprets `Close` instead
    /// of forwarding it.
    #[test]
    fn esc_leaves_only_from_the_index() {
        let mut a = FakeSection::new(Flow::Close(false));
        let mut b = FakeSection::new(Flow::Close(false));
        let mut shell = Shell::new(vec![
            section("Session", 's', &mut a),
            section("Backends", 'b', &mut b),
        ]);

        shell.key(Key::Enter);
        assert_eq!(shell.key(Key::Esc), Flow::Stay, "back to the index");
        assert_eq!(shell.key(Key::Esc), Flow::Close(false), "now it leaves");
    }

    /// **A read-only section's visit is not an edit** (#2009 PR10b).
    ///
    /// The case the per-section flag exists for, now that a second section is
    /// hosted: an operator opens Session and changes nothing, then reads the
    /// Permissions audit and leaves. Asking the shell "did anything apply"
    /// still answers no here — but it answers no by luck, because the
    /// read-only section never reports `true`. Asking SESSION answers no
    /// because Session is the thing that did not change, which stays correct
    /// when a writing section joins the index.
    #[test]
    fn a_read_only_sections_visit_is_not_attributed_to_the_editing_one() {
        let mut session = FakeSection::new(Flow::Close(false));
        let mut audit = FakeSection::new(Flow::Close(false));
        let mut shell = Shell::new(vec![
            section("Session", 's', &mut session),
            section("Permissions", 'p', &mut audit),
        ]);

        shell.key(Key::Char('p'));
        shell.key(Key::Enter); // read the audit, leave
        assert!(!shell.section_applied(0), "Session was never opened");
        assert!(!shell.section_applied(1), "and a read applies nothing");
        assert_eq!(shell.key(Key::Esc), Flow::Close(false));
    }

    /// ...and the converse: an edit in Session survives a detour through a
    /// read-only section, and is still attributed to Session.
    #[test]
    fn an_edit_survives_a_detour_through_a_read_only_section() {
        let mut session = FakeSection::new(Flow::Close(true));
        let mut audit = FakeSection::new(Flow::Close(false));
        let mut shell = Shell::new(vec![
            section("Session", 's', &mut session),
            section("Permissions", 'p', &mut audit),
        ]);

        shell.key(Key::Char('s'));
        shell.key(Key::Enter); // edit and close
        shell.key(Key::Char('p'));
        shell.key(Key::Enter); // detour through the audit

        assert!(shell.section_applied(0), "the edit is still Session's");
        assert!(!shell.section_applied(1), "the detour claimed nothing");
        assert_eq!(shell.key(Key::Esc), Flow::Close(true));
    }

    /// **A LINK row closes the shell and names itself** (§3.5 answer 3).
    ///
    /// The row is in the index — that is the point of LINK mode — but entering
    /// it hands back to the caller instead of opening in place, because the
    /// surface it names still owns state the shell cannot reach.
    #[test]
    fn a_link_row_closes_the_shell_and_reports_which_row() {
        let mut a = FakeSection::new(Flow::Stay);
        let mut shell = Shell::new(vec![section("Session", 's', &mut a), link("Backends", 'b')]);

        assert_eq!(shell.linked(), None);
        assert_eq!(shell.key(Key::Char('b')), Flow::Close(false));
        assert_eq!(shell.linked(), Some(1), "the caller learns WHICH link");
    }

    /// A link carries an earlier section's applied flag out with it — walking
    /// on to another surface does not discard the edit already made.
    #[test]
    fn a_link_row_carries_out_what_an_earlier_section_applied() {
        let mut a = FakeSection::new(Flow::Close(true));
        let mut shell = Shell::new(vec![section("Session", 's', &mut a), link("Backends", 'b')]);

        shell.key(Key::Enter); // open Session
        shell.key(Key::Enter); // the section applies and closes
        assert!(shell.section_applied(0));
        assert_eq!(shell.key(Key::Char('b')), Flow::Close(true));
    }

    /// A shell whose only row is a LINK still shows the index: passing through
    /// would close before the operator saw anything.
    #[test]
    fn a_lone_link_row_is_not_a_pass_through() {
        let shell = Shell::new(vec![link("Backends", 'b')]);
        assert_eq!(shell.view, View::Index);
    }

    /// **A section's applied flag is its own** — the property that lets each
    /// caller read its own panel's outcome.
    #[test]
    fn applied_is_tracked_per_section() {
        let mut a = FakeSection::new(Flow::Close(false));
        let mut b = FakeSection::new(Flow::Close(true));
        let mut shell = Shell::new(vec![
            section("Session", 's', &mut a),
            section("Backends", 'b', &mut b),
        ]);

        shell.key(Key::Char('b'));
        shell.key(Key::Enter);
        assert!(!shell.section_applied(0), "Session applied nothing");
        assert!(shell.section_applied(1), "Backends did");
        assert!(
            !shell.section_applied(99),
            "an absent section applied nothing"
        );
    }

    /// **An index of one is not an index.** A single-section shell IS the
    /// panel: it opens into it and closes when it closes, so `/settings` is
    /// unchanged until a second section exists to choose between.
    #[test]
    fn a_single_section_shell_is_the_panel() {
        let mut a = FakeSection::new(Flow::Close(true));
        let mut shell = Shell::new(vec![section("Session", 's', &mut a)]);

        assert_eq!(shell.view, View::Section(0), "no index to pass through");
        assert_eq!(
            shell.key(Key::Esc),
            Flow::Close(true),
            "the section closing closes the shell, and carries its applied flag"
        );
    }

    /// ...and the rule self-cancels: add a section and the index appears.
    #[test]
    fn a_second_section_brings_the_index_back() {
        let mut a = FakeSection::new(Flow::Close(false));
        let mut b = FakeSection::new(Flow::Close(false));
        let shell = Shell::new(vec![
            section("Session", 's', &mut a),
            section("Backends", 'b', &mut b),
        ]);
        assert_eq!(shell.view, View::Index, "two sections means a choice");
    }

    /// An edit is not undone by leaving through the index.
    #[test]
    fn an_applied_section_is_remembered_after_returning_to_the_index() {
        let mut a = FakeSection::new(Flow::Close(true));
        let mut b = FakeSection::new(Flow::Close(false));
        let mut shell = Shell::new(vec![
            section("Session", 's', &mut a),
            section("Backends", 'b', &mut b),
        ]);

        shell.key(Key::Char('s'));
        shell.key(Key::Enter); // the section applies and closes
        assert!(shell.section_applied(0));

        // A second section that applies nothing must not clear it — and must
        // not CLAIM it either: the flag belongs to the section that earned it.
        shell.key(Key::Char('b'));
        shell.key(Key::Enter);
        assert!(shell.section_applied(0), "one section's edit is its own");
        assert!(
            !shell.section_applied(1),
            "and is not attributed to the section that applied nothing"
        );
        assert_eq!(
            shell.key(Key::Esc),
            Flow::Close(true),
            "the shell reports any"
        );
    }

    /// Accelerators open a section from the index without moving the cursor.
    #[test]
    fn an_accelerator_opens_its_section_from_anywhere_in_the_index() {
        let mut a = FakeSection::new(Flow::Stay);
        let mut b = FakeSection::new(Flow::Stay);
        let mut shell = Shell::new(vec![
            section("Session", 's', &mut a),
            section("Backends", 'b', &mut b),
        ]);

        shell.key(Key::Char('B'));
        assert_eq!(shell.view, View::Section(1), "case-insensitive accelerator");
    }

    /// **Every key belongs to the section while one is open**, including keys
    /// the index binds. A shell that kept `/` or `j` for itself would steal
    /// them from a panel's own text entry.
    #[test]
    fn a_focused_section_receives_every_key_including_the_shells_own() {
        let mut a = FakeSection::new(Flow::Stay);
        let mut b = FakeSection::new(Flow::Stay);
        let sent = [Key::Char('/'), Key::Char('j'), Key::Char('s'), Key::Up];
        {
            let mut shell = Shell::new(vec![
                section("Session", 's', &mut a),
                section("Backends", 'b', &mut b),
            ]);
            shell.key(Key::Enter);
            for key in sent {
                shell.key(key);
            }
            assert_eq!(shell.view, View::Section(0), "it stayed open");
        }
        // Asserted after the shell's borrow ends, which is also the proof that
        // the shell holds the panel rather than a copy of it.
        assert_eq!(
            a.keys.borrow().as_slice(),
            sent.as_slice(),
            "every key reached the section, including the index's own bindings"
        );
    }

    #[test]
    fn the_filter_narrows_the_index_and_esc_drops_the_lens() {
        let mut a = FakeSection::new(Flow::Stay);
        let mut b = FakeSection::new(Flow::Stay);
        let mut c = FakeSection::new(Flow::Stay);
        let mut shell = Shell::new(vec![
            section("Session", 's', &mut a),
            section("Backends", 'b', &mut b),
            section("Context", 'c', &mut c),
        ]);

        shell.key(Key::Char('/'));
        shell.key(Key::Char('c'));
        // `c` is in "Ba(c)kends" too — a substring filter matches anywhere,
        // which is the point, so the narrowing case needs a real needle.
        assert_eq!(shell.matches(), vec![1, 2], "`c` matches both");
        shell.key(Key::Char('o'));
        assert_eq!(shell.matches(), vec![2], "`co` matches Context only");

        // A filter that matches nothing opens nothing rather than closing.
        shell.key(Key::Char('z'));
        assert_eq!(
            shell.matches(),
            Vec::<usize>::new(),
            "no section matches `coz`"
        );
        assert_eq!(shell.key(Key::Enter), Flow::Stay, "nothing to open");
        assert_eq!(shell.view, View::Filtering("coz".to_string()));

        shell.key(Key::Backspace);
        assert_eq!(shell.matches(), vec![2]);
        shell.key(Key::Enter);
        assert_eq!(
            shell.view,
            View::Section(2),
            "Enter opens the MATCH, not row 0"
        );
    }

    /// Esc from a filter clears it rather than leaving the shell — "back"
    /// drops the lens first.
    #[test]
    fn esc_while_filtering_clears_the_filter_and_stays() {
        let mut a = FakeSection::new(Flow::Stay);
        let mut b = FakeSection::new(Flow::Stay);
        let mut shell = Shell::new(vec![
            section("Session", 's', &mut a),
            section("Backends", 'b', &mut b),
        ]);

        shell.key(Key::Char('/'));
        shell.key(Key::Char('z'));
        assert_eq!(shell.key(Key::Esc), Flow::Stay);
        assert_eq!(
            shell.view,
            View::Index,
            "the lens is gone, the shell is not"
        );
        assert_eq!(shell.key(Key::Esc), Flow::Close(false), "now it leaves");
    }

    /// The filter takes letters the index binds, so a search for "backends"
    /// is not eaten by `j`/`k` movement or an accelerator.
    #[test]
    fn filtering_takes_letters_the_index_would_otherwise_bind() {
        let mut a = FakeSection::new(Flow::Stay);
        let mut b = FakeSection::new(Flow::Stay);
        let mut shell = Shell::new(vec![
            section("Session", 's', &mut a),
            section("Backends", 'b', &mut b),
        ]);

        shell.key(Key::Char('/'));
        for c in "backends".chars() {
            shell.key(Key::Char(c));
        }
        assert_eq!(shell.view, View::Filtering("backends".to_string()));
        assert_eq!(
            shell.matches(),
            vec![1],
            "the whole word was typed, not bound"
        );
    }

    /// The index renders one row per section, with its accelerator, and the
    /// section draws instead once it is open.
    #[test]
    fn the_index_renders_its_sections_and_a_section_takes_over() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut a = FakeSection::new(Flow::Stay);
        let mut b = FakeSection::new(Flow::Stay);
        let mut shell = Shell::new(vec![
            Section {
                name: "Session",
                accel: 's',
                summary: "dials, editor, reasoning".to_string(),
                body: Body::Screen(&mut a),
            },
            section("Backends", 'b', &mut b),
        ]);

        let mut term = Terminal::new(TestBackend::new(72, 8)).unwrap();
        term.draw(|f| shell.draw(f)).unwrap();
        let rendered: String = {
            let buf = term.backend().buffer();
            (0..8)
                .map(|y| {
                    (0..72)
                        .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(rendered.contains("Session"), "{rendered}");
        assert!(
            rendered.contains("[s]"),
            "the accelerator is shown: {rendered}"
        );
        assert!(rendered.contains("Esc leave"), "{rendered}");

        shell.key(Key::Enter);
        term.draw(|f| shell.draw(f)).unwrap();
        drop(shell);
        assert_eq!(
            *a.drawn.borrow(),
            1,
            "the open section drew exactly once, and the index did not"
        );
    }
}
