//! **One inline-panel driver, for every panel.**
//!
//! `/psyche` and `/backends` each carried their own event loop, and the two
//! were near-verbatim: the same `PanelRawGuard::enter`, the same terminal
//! build and `clear`, the same 250 ms `event::poll`, the same
//! `KeyEventKind::Press` filter, the same CONTROL extraction, the same
//! trailing `clear`, the same scoped-so-the-restore-lands-here comment. They
//! differed in exactly two things — **the key table** and **the outcome
//! type** — which is precisely what [`Screen::key`] and the caller's own
//! return value parameterize.
//!
//! That is the reuse discipline's own test, applied before adding a third
//! panel rather than after: a second implementation is warranted only when
//! the existing abstraction cannot be widened to cover the new case, and
//! nothing in either loop resisted this one. The measured hazard is in
//! CLAUDE.md — five spinners, four erase strategies, three animation clocks —
//! and a third copy of a raw-mode loop is how a `/settings` panel would have
//! inherited whatever the other two get wrong next.
//!
//! # What the driver owns, and what it must not
//!
//! It owns the terminal lifecycle: raw mode, the viewport, the repaint
//! cadence, and giving the terminal back. It owns no policy — no key means
//! anything here, and no panel's outcome is interpreted. A driver that
//! started deciding what Enter means would be back to one loop per panel,
//! wearing a shared name.
//!
//! # Where a panel draws
//!
//! Under the cockpit the presenter LENDS reserved rows on the real terminal
//! (`SurfaceRequest::Panel`), because fd 1 is a pty slave the cockpit is
//! capturing and a panel that draws there never reaches the screen. Off the
//! cockpit a panel takes the bottom rows of stdout as it always has. Both
//! arrive here as one `Option<&PanelWindow>`, so no panel implements that
//! choice twice.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use newt_core::tty::raw_mode::RawModeGuard;

use crate::inline_viewport::InlineTerm;
use crate::session_worker::PanelWindow;

/// What one keypress did to the panel.
///
/// Deliberately two-armed. A panel closes because the operator ACCEPTED
/// something or because they did not, and the boolean is that distinction —
/// not a success/failure code. What an acceptance then MEANS is the panel's
/// own business (`close_outcome` in each module), which is why nothing here
/// looks inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    /// Keep driving.
    Stay,
    /// Close the panel; `true` when the operator explicitly applied.
    Close(bool),
}

/// A panel: something that draws itself and answers keys.
///
/// The whole surface a panel must implement to be driven. Note what is
/// ABSENT — no terminal, no raw mode, no poll interval, no repaint. A panel
/// that reached for those would be a second driver.
pub(crate) trait Screen {
    /// Render the current state into the panel's rows.
    fn draw(&self, frame: &mut ratatui::Frame);

    /// Handle one key press. `ctrl` is pre-extracted because every existing
    /// key table needed it and each was extracting it identically.
    fn key(&mut self, code: KeyCode, ctrl: bool) -> Flow;
}

/// Restore the terminal on EVERY exit path of a panel — return, error, panic.
///
/// **RAII, not happy-path control flow**, and that distinction is the whole
/// point (#1889). Both panels used to call `enable_raw_mode()` and then
/// `disable_raw_mode()` as a STATEMENT after the loop closure. An error return
/// reached it; a **panic unwound straight past**, leaving the operator in a
/// shell with no echo and no line discipline — a session that looks broken,
/// recoverable only with `reset`.
///
/// `SplashScreenGuard`'s doc (#1411) enumerated the crate's raw-mode pairs and
/// called the splash "the only one with no guard at all". These two panels
/// were not in that count. This is the conversion.
///
/// The guard binds BEFORE the fallible call — the ordering
/// `AltScreenGuard::enter` pays for and `InlineGuard::enter` repeats: from that
/// point the restore is owed regardless of what the next line does.
///
/// `enter_panel_raw_mode_is_the_only_way_in` pins that neither panel reaches
/// past this type to crossterm; `panel_raw_mode_pty_test` proves the Drop
/// against a real tty, from a parent that outlives the panicking child.
///
/// **SUBSUMED onto [`RawModeGuard`] (#1905).** This owned raw mode and nothing
/// else, through crossterm's `enable_raw_mode`/`disable_raw_mode` — which keep
/// ONE process-global "mode prior to raw" and so restore to a fixed state
/// rather than to what this guard found. C2b (#1891) hit that as a live defect
/// in `InlineGuard`: an inner frame closing handed the terminal back to cooked
/// while the outer frame was still up. `RawModeGuard` captures the prior
/// termios at `enter` and restores exactly that, so nesting composes.
///
/// A newtype rather than a plain alias, because the panels' own tests and the
/// PTY child name this type, and because the doc above is about the panels.
///
/// Moved here from `config_panel` with the driver it belongs to, unchanged:
/// the guard is part of running a panel, and leaving it behind in one panel's
/// module while both panels used it was the same split this file closes.
pub(crate) struct PanelRawGuard {
    _raw: RawModeGuard,
}

impl PanelRawGuard {
    pub(crate) fn enter() -> io::Result<Self> {
        Ok(Self {
            _raw: RawModeGuard::enter()?,
        })
    }
}

/// #1950: through the ONE inline constructor, so a terminal that will not
/// answer `ESC[6n` anchors the panel instead of refusing to open it.
/// #1979: leases its rows with `OnCollision::Shift`. A panel opens while the
/// prompt viewport is live and bottom-pinned, so asking for the bottom rows
/// outright is #1977 — the panel drew and the prompt's next repaint erased its
/// body. Shifting mints the nearest free rows ABOVE the holder, which is the
/// anchor-above-the-prompt fix expressed as the mint's policy rather than as a
/// special case in the fallback.
pub(crate) fn make_terminal(height: u16) -> io::Result<InlineTerm> {
    let lease =
        crate::inline_viewport::lease_bottom_rows(height, newt_core::tty::OnCollision::Shift)?;
    crate::inline_viewport::inline_terminal(lease)
}

/// Run `screen` until it closes, and report whether the operator applied.
///
/// The raw-mode guard is scoped so its restore lands where the original bare
/// statement did. An error mid-loop returns `Err` with the guard already
/// dropped and the terminal handed back — the caller keeps whatever state its
/// screen accumulated, which is how `backend_panel` still reports file
/// operations that committed before a terminal failure.
///
/// # Errors
///
/// The terminal could not be taken, built, polled, read or repainted.
pub(crate) fn drive(
    screen: &mut dyn Screen,
    height: u16,
    window: Option<&PanelWindow>,
) -> io::Result<bool> {
    let mut applied = false;
    let loop_result = {
        let _raw = PanelRawGuard::enter()?;
        (|| -> io::Result<()> {
            let mut terminal = match window {
                Some(window) => window.terminal()?,
                None => make_terminal(height)?,
            };
            terminal.clear()?;
            loop {
                terminal.draw(|f| screen.draw(f))?;
                // A poll rather than a blocking read: the panel repaints on a
                // cadence so a status line or a spinner can change without a
                // keypress, and a timed-out poll is not an event.
                if !event::poll(Duration::from_millis(250))? {
                    continue;
                }
                let Event::Key(key) = event::read()? else {
                    continue;
                };
                // Release and Repeat are not presses. Without this filter a
                // terminal that reports both delivers every key twice.
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                if let Flow::Close(apply) = screen.key(key.code, ctrl) {
                    applied = apply;
                    break;
                }
            }
            terminal.clear()?;
            Ok(())
        })()
    };
    loop_result.map(|()| applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The structural half of #1889.** The PTY test proves `PanelRawGuard`
    /// restores; this proves the panels go through it.
    ///
    /// Without it the two tests together are still vacuous in the way that
    /// matters: a guard can be correct and unused, which is exactly the state
    /// these files were in before — `enable_raw_mode()` called directly, the
    /// restore a statement a panic skips. Counted over the source because the
    /// property is "no other path exists", and absence is not observable by
    /// calling something.
    #[test]
    fn enter_panel_raw_mode_is_the_only_way_in() {
        // PRODUCTION code only, and not merely for tidiness: this test lives
        // IN config_panel.rs, so `include_str!` pulls in its own needles and
        // the first run read 2 enables where there is one. Cutting at the
        // test module is also the right scope — the property is that no
        // production path reaches raw mode except through the guard.
        //
        // #1898 hardened the cut: the version that shipped here split at the
        // FIRST `#[cfg(test)]` anywhere and fell back to "" when it found
        // none. `rich_input.rs` has an inline one 700 lines early, and "" makes
        // every `count() == 0` assertion pass having read nothing.
        // All THREE files now: the guard moved here with the driver, and the
        // property — no production path reaches raw mode except through it —
        // is only meaningful if the panels are scanned too.
        let driver = crate::production_source(include_str!("panel.rs"));
        let config = crate::production_source(include_str!("config_panel.rs"));
        let backend = crate::production_source(include_str!("backend_panel.rs"));
        // Count CALL forms, not the name: the guard's own doc comment
        // discusses `enable_raw_mode()` and `disable_raw_mode()` in prose, and
        // a test that counted mentions would move every time someone edited a
        // comment — noise that trains people to adjust the number.
        // #1905 SUBSUMED the raw half onto `RawModeGuard`, so the count that
        // was "exactly one" is now "none" in BOTH files. The one nesting-aware
        // owner lives in newt-core; a bare crossterm call reappearing here
        // would be a second owner restoring to a fixed state rather than to
        // what it found — the defect C2b hit in `InlineGuard`.
        // COMMENTS STRIPPED, not call-shapes matched. The original counted
        // `enable_raw_mode()?` exactly because the guard's own doc discusses
        // both names in prose — and that doc now lives in THIS file, so a
        // bare-name count reads its own explanation as a violation. Dropping
        // comment lines first says what is actually meant ("no call, in any
        // form"), and stops a future edit to the prose from moving a number.
        let code_only = |source: &str| -> String {
            source
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        for (name, source) in [
            ("panel", &driver),
            ("config_panel", &config),
            ("backend_panel", &backend),
        ] {
            let code = code_only(source);
            assert_eq!(
                code.matches("enable_raw_mode(").count(),
                0,
                "{name}: raw mode comes from RawModeGuard, never crossterm directly"
            );
            assert_eq!(
                code.matches("disable_raw_mode(").count(),
                0,
                "{name}: a bare disable means a restore that a panic skips"
            );
        }
        assert!(
            driver.contains("_raw: RawModeGuard"),
            "PanelRawGuard must HOLD a RawModeGuard — composition, not a \
             reimplementation"
        );
        // And the loop itself is gone from both panels: a panel that grew its
        // own `event::poll` back would be the third copy this file removed.
        for (name, source) in [("config_panel", &config), ("backend_panel", &backend)] {
            assert_eq!(
                source.matches("event::poll(").count(),
                0,
                "{name}: the event loop belongs to panel::drive"
            );
        }
        // The guard itself must still be a Drop obligation. A guard whose
        // restore moved into an inherent method would satisfy every count
        // above and leak on unwind again.
        // No `impl Drop for PanelRawGuard` any more, and its absence is the
        // point: the restore is the FIELD's, so there is nothing here to
        // forget. A hand-written Drop reappearing would mean a second restore
        // racing the field's.
        assert!(
            !driver.contains("impl Drop for PanelRawGuard"),
            "the restore is RawModeGuard's; a Drop impl here would be a second one"
        );
    }

    /// A screen that records what it was asked and closes on demand — no
    /// terminal, so the DISPATCH is testable without a TTY even though
    /// [`drive`] itself is not.
    #[derive(Default)]
    struct Recorder {
        seen: Vec<(KeyCode, bool)>,
        close_on: Option<(KeyCode, bool)>,
    }

    impl Screen for Recorder {
        fn draw(&self, _frame: &mut ratatui::Frame) {}

        fn key(&mut self, code: KeyCode, ctrl: bool) -> Flow {
            self.seen.push((code, ctrl));
            match self.close_on {
                Some((c, applied)) if c == code => Flow::Close(applied),
                _ => Flow::Stay,
            }
        }
    }

    /// `Flow` says one thing, and a panel cannot accidentally say it by
    /// returning a bare bool from `key` — the two arms are not both `bool`.
    #[test]
    fn a_close_carries_whether_the_operator_applied() {
        let mut screen = Recorder {
            close_on: Some((KeyCode::Enter, true)),
            ..Recorder::default()
        };
        assert_eq!(screen.key(KeyCode::Up, false), Flow::Stay);
        assert_eq!(screen.key(KeyCode::Enter, false), Flow::Close(true));

        let mut cancelled = Recorder {
            close_on: Some((KeyCode::Esc, false)),
            ..Recorder::default()
        };
        assert_eq!(cancelled.key(KeyCode::Esc, false), Flow::Close(false));
        assert_eq!(
            cancelled.seen,
            vec![(KeyCode::Esc, false)],
            "the screen sees the key it closed on"
        );
    }

    /// The ctrl flag reaches the screen, because every key table needed it
    /// and each was extracting it from `KeyModifiers` identically.
    #[test]
    fn the_screen_is_told_about_control() {
        let mut screen = Recorder::default();
        screen.key(KeyCode::Char('s'), true);
        screen.key(KeyCode::Char('s'), false);
        assert_eq!(
            screen.seen,
            vec![(KeyCode::Char('s'), true), (KeyCode::Char('s'), false)]
        );
    }
}
