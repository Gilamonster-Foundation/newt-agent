//! **RichTUI free-text modal for a pending clarification batch** (#2524
//! item 7).
//!
//! `interaction_view::ModalInput` assumes a fixed set of discrete choices
//! (`ControlKind::Choice`); a clarification batch is answered in free text —
//! an ordinal (`2: ...`), `/discuss`, `/new`, or a bare `yes` taking an
//! outstanding proposal — so this is a small purpose-built reader instead of
//! stretching that widget to cover a shape it was not built for (Option 1 of
//! the addendum to #2515, approved by Shawn 2026-09-22).
//!
//! It draws the SAME [`crate::modal::frame`] chrome every other modal in this
//! crate wears. `/discuss` and the ordinal shape are named in the chrome's
//! HINT rather than appended to the scrollable batch text — the F4-inspect
//! bug #2524 records for that pattern (a hint folded into scrollable content
//! scrolls away with it instead of staying pinned).
//!
//! Inline viewport only, never the alternate screen —
//! `docs/decisions/plain_scroller_tui.md`; this module follows
//! `interaction_view`'s split exactly (a pure `requested_rows`/`draw` pair
//! usable in a unit test, and a terminal-owning `present`/`present_in` pair
//! gated to `rich-tui`).

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{Clear, ClearType};
use newt_core::tty::raw_mode::RawModeGuard;
use newt_core::tty::OnCollision;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::chat::ReadOutcome;
use crate::inline_viewport::InlineTerm;

/// Rows the modal needs: the wrapped batch, two borders, a hint legend, and
/// one row for the free-text answer.
pub(crate) fn requested_rows(batch: &str, cols: u16) -> u16 {
    let width = usize::from(cols.saturating_sub(2)).max(1);
    let body_rows: usize = batch
        .lines()
        .map(|line| newt_core::tty::wrap_line(line, width).len().max(1))
        .sum();
    u16::try_from(body_rows + 4).unwrap_or(u16::MAX)
}

/// One free-text read, in progress: what the operator has typed so far.
struct FreeTextInput {
    answer: String,
}

/// What one key/paste event did to the read in progress.
enum Step {
    /// Nothing yet — keep reading.
    Continue,
    Done(ReadOutcome),
}

impl FreeTextInput {
    fn event(&mut self, event: Event) -> Step {
        match event {
            Event::Paste(text) => {
                self.answer
                    .extend(text.chars().filter(|ch| !ch.is_control()));
                Step::Continue
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Char('c' | 'd') if ctrl => {
                        let outcome = if key.code == KeyCode::Char('c') {
                            ReadOutcome::Interrupted
                        } else {
                            ReadOutcome::Eof
                        };
                        Step::Done(outcome)
                    }
                    KeyCode::Enter => {
                        Step::Done(ReadOutcome::Line(std::mem::take(&mut self.answer)))
                    }
                    KeyCode::Backspace => {
                        self.answer.pop();
                        Step::Continue
                    }
                    KeyCode::Char(typed) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                        self.answer.push(typed);
                        Step::Continue
                    }
                    _ => Step::Continue,
                }
            }
            _ => Step::Continue,
        }
    }
}

/// Draw the chrome + batch + answer line into `frame`.
fn draw(frame: &mut ratatui::Frame, batch: &str, hint: &str, answer: &str) {
    let body = crate::modal::frame(
        frame,
        frame.area(),
        &crate::modal::Chrome {
            title: "clarification",
            subtitle: Some("· 1 of 1".to_string()),
            hint: Some(hint),
        },
    );
    let [text, answer_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(body);
    let lines: Vec<Line<'static>> = batch.lines().map(|l| Line::from(l.to_string())).collect();
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), text);
    let shown = format!("> {answer}");
    frame.render_widget(Paragraph::new(shown.as_str()), answer_area);
    if answer_area.width > 0 && answer_area.height > 0 {
        frame.set_cursor_position((
            (answer_area.x + newt_core::tty::str_width(&shown) as u16)
                .min(answer_area.x + answer_area.width.saturating_sub(1)),
            answer_area.y,
        ));
    }
}

/// Run the modal's event loop on an already-positioned terminal (the
/// cockpit's real-terminal handoff, mirroring `interaction_view::present_in`).
fn run(terminal: &mut InlineTerm, batch: &str, hint: &str) -> io::Result<ReadOutcome> {
    let mut input = FreeTextInput {
        answer: String::new(),
    };
    loop {
        terminal.draw(|f| draw(f, batch, hint, &input.answer))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        if let Step::Done(outcome) = input.event(event::read()?) {
            return Ok(outcome);
        }
    }
}

/// Present the modal on the terminal `terminal` already occupies (the
/// cockpit's saved real terminal — the presenter owns row reservation and
/// cleanup; this loop owns input until dismissal). The `_inline`
/// sibling this is, is [`present`] below.
pub(crate) fn present_in(
    terminal: &mut InlineTerm,
    batch: &str,
    hint: &str,
) -> io::Result<ReadOutcome> {
    run(terminal, batch, hint)
}

/// Present the modal on a freshly leased region of the classic (non-cockpit)
/// terminal, mirroring `interaction_view::present`.
pub(crate) fn present(batch: &str, hint: &str) -> io::Result<ReadOutcome> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let height = requested_rows(batch, cols).min(rows).max(1);
    let _raw = RawModeGuard::enter()?;
    let lease = crate::inline_viewport::lease_bottom_rows(height, OnCollision::Shift)?;
    let mut terminal = crate::inline_viewport::inline_terminal(lease)?;
    terminal.clear()?;
    let outcome = run(&mut terminal, batch, hint);
    // Erase the reserved region before handing the terminal back, or the next
    // committed line prints over a live modal frame — `interaction_view::
    // InlineGuard`'s drop does the same thing for the same reason.
    let mut out = io::stdout();
    let _ = crossterm::execute!(
        out,
        crossterm::cursor::MoveToColumn(0),
        Clear(ClearType::FromCursorDown)
    );
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn rendered(batch: &str, hint: &str, answer: &str, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, batch, hint, answer)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    /// **The rich path renders the batch inside chrome with `/discuss` in
    /// the hint** — the red-first assertion for item 7's rendering half.
    #[test]
    fn the_batch_renders_inside_chrome_with_discuss_in_the_hint() {
        let rows = rendered(
            "1: which lane?\n   a) build\n   b) ship",
            "reply with an ordinal — or /discuss to talk it through",
            "",
            60,
            10,
        );
        let text = rows.join("\n");
        assert!(text.contains("clarification"), "title: {text}");
        assert!(text.contains("1: which lane?"), "batch body: {text}");
        assert!(text.contains("/discuss"), "hint names /discuss: {text}");
        assert!(rows[0].starts_with('╭'), "modal chrome edge: {:?}", rows[0]);
    }

    /// The typed answer shows on the answer row, prefixed the same way
    /// `ModalInput`'s free-text mode prefixes its own — `> `.
    #[test]
    fn the_typed_answer_is_shown_on_the_answer_row() {
        let rows = rendered("1: which lane?", "hint", "2", 40, 6);
        assert!(rows.iter().any(|r| r.contains("> 2")), "{rows:?}");
    }

    #[test]
    fn requested_rows_grows_with_a_wrapped_batch() {
        let short = requested_rows("one line", 40);
        let long = requested_rows(&"x".repeat(200), 40);
        assert!(long > short);
    }

    #[test]
    fn enter_submits_the_typed_answer() {
        let mut input = FreeTextInput {
            answer: "2".to_string(),
        };
        let step = input.event(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert!(matches!(step, Step::Done(ReadOutcome::Line(a)) if a == "2"));
    }

    #[test]
    fn ctrl_c_interrupts_and_ctrl_d_is_eof() {
        let mut input = FreeTextInput {
            answer: String::new(),
        };
        let ctrl_c = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        ));
        assert!(matches!(
            input.event(ctrl_c),
            Step::Done(ReadOutcome::Interrupted)
        ));
        let ctrl_d = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        ));
        assert!(matches!(input.event(ctrl_d), Step::Done(ReadOutcome::Eof)));
    }

    #[test]
    fn typed_text_accumulates_and_backspace_removes_it() {
        let mut input = FreeTextInput {
            answer: String::new(),
        };
        for ch in ['/', 'd', 'i', 's', 'c'] {
            input.event(Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
        assert_eq!(input.answer, "/disc");
        input.event(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )));
        assert_eq!(input.answer, "/dis");
    }
}
