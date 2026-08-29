//! **The wizard's operator, after the `Console` trait** (D1b-3, #1913).
//!
//! `line_console::Console` carried four methods, and the two that mattered
//! were the problem: `ask(&str) -> String` transported a PRE-RENDERED string,
//! so a prompt could exist with no definition behind it, and `ask_secret`
//! existed as a second reader precisely because a string cannot say "this one
//! is hidden". That second reader is where `Echo::Stars` lived, and it is why
//! `read_line_raw` existed at all.
//!
//! What replaces it is not a smaller console. It is **C1's seam plus a place
//! to narrate**:
//!
//! * [`Operator::ask`] takes an [`InteractionDefinition`] and returns what the
//!   terminal adapter reports. Masking is DERIVED from the controls by
//!   `present_on_terminal`, so there is no `ask_secret` to have — a secret is
//!   a definition with a `ControlKind::Secret` in it.
//! * [`Operator::say`] narrates. Narration is not an interaction and never
//!   was; giving it a method on the ask path is what made `Console` look like
//!   one thing.
//!
//! ## It cannot carry a reader
//!
//! D1b-1 moved masking into the shared adapter so `read_line_raw` could die.
//! The way that gets undone is a replacement injection point that accepts a
//! reader — a test double supplying "just a simpler way to read a line" —
//! after which one flow reads without the adapter and nothing notices.
//!
//! So neither constructor takes one. [`Operator::terminal`] is the same
//! closure `LeanSurface`, `RichSurface` and `crew_form` use, and
//! [`Operator::scripted`] takes ANSWERS, not a way of obtaining them. There is
//! no third constructor and the fields are private, so a private echo path is
//! not something a caller can supply.

#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::collections::VecDeque;
use std::io;

use newt_core::interaction_surface::SurfaceInteraction;
use newt_core::HumanQuestionOutcome;
use newt_interaction::InteractionDefinition;

/// How the wizard reaches the operator.
///
/// Borrowed closures rather than a trait: there is one production
/// implementation and one test implementation, and a trait would invite a
/// third. `Fn` rather than `FnMut`, so every wizard step takes `&Operator` and
/// the wizard never has to thread `&mut` through thirty signatures to print a
/// line.
pub(super) struct Operator<'a> {
    ask: Box<dyn Fn(&SurfaceInteraction) -> HumanQuestionOutcome + 'a>,
    say: Box<dyn Fn(&str) + 'a>,
}

impl Operator<'static> {
    /// The real terminal.
    ///
    /// Byte-identical to `LeanSurface::present_interaction`,
    /// `RichSurface::present_interaction` and `crew_form::run_edit`: acquire
    /// the sealed window, present through the one adapter. Nothing here knows
    /// how to read a key.
    ///
    /// This also retires `FirstRunConsole`. It existed so Esc and Ctrl-C
    /// surfaced as catchable `io::Error`s during first-run setup instead of
    /// SIGINT killing the process mid-wizard — and the seam already does
    /// that, reporting `Cancelled` and `ExitRequested` as outcomes. The
    /// distinction the first-run path needed is now the default.
    pub(super) fn terminal() -> Self {
        Self {
            ask: Box::new(|interaction| {
                let window = newt_core::tty::Terminal::suspend_for_prompt();
                crate::permissions::present_on_terminal(&window, interaction)
            }),
            say: Box::new(|line| println!("{line}")),
        }
    }
}

impl<'a> Operator<'a> {
    /// Present `definition` and return the submitted line.
    ///
    /// # Errors
    ///
    /// Esc/Ctrl-C as [`io::ErrorKind::Interrupted`], EOF as
    /// [`io::ErrorKind::UnexpectedEof`], and an absent or broken operator as
    /// [`io::Error::other`] — the distinctions a bare `read_line` cannot
    /// make, which is why the wizard gets them for free now.
    pub(super) fn ask(&self, definition: &InteractionDefinition) -> io::Result<String> {
        match (self.ask)(&SurfaceInteraction::blocking(definition.clone())) {
            HumanQuestionOutcome::Answer(line) => Ok(line.trim().to_string()),
            HumanQuestionOutcome::Cancelled | HumanQuestionOutcome::ExitRequested => {
                Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
            }
            HumanQuestionOutcome::InputClosed => {
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"))
            }
            HumanQuestionOutcome::Unavailable | HumanQuestionOutcome::InputFailed => {
                Err(io::Error::other("no operator available"))
            }
        }
    }

    /// Narrate one line.
    pub(super) fn say(&self, line: &str) {
        (self.say)(line);
    }
}

/// A scripted operator for tests: answers in order, narration recorded.
///
/// `#[cfg(test)]`, so the production binary carries no way to drive the
/// wizard from a canned list.
///
/// Takes ANSWERS, not a reader — see this module's note. Running out returns
/// an empty answer rather than EOF, matching the `ScriptedConsole` it
/// replaces, so the wizard's existing scripts keep their meaning.
#[cfg(test)]
pub(super) struct Script {
    answers: RefCell<VecDeque<String>>,
    /// Everything the wizard narrated, and every prompt it rendered.
    pub(super) output: RefCell<Vec<String>>,
}

#[cfg(test)]
impl Script {
    pub(super) fn new(answers: &[&str]) -> Self {
        Self {
            answers: RefCell::new(answers.iter().map(|a| (*a).to_string()).collect()),
            output: RefCell::new(Vec::new()),
        }
    }

    /// An [`Operator`] driven by this script.
    pub(super) fn operator(&self) -> Operator<'_> {
        Operator {
            ask: Box::new(move |interaction| {
                self.output
                    .borrow_mut()
                    .push(newt_core::markup::plain::render(&interaction.definition));
                HumanQuestionOutcome::Answer(
                    self.answers.borrow_mut().pop_front().unwrap_or_default(),
                )
            }),
            say: Box::new(move |line| self.output.borrow_mut().push(line.to_string())),
        }
    }

    /// The next answer the script would hand over, without consuming it.
    ///
    /// Two authentication-retry tests assert that a REJECTED key was not
    /// followed by another prompt — that the wizard stopped rather than
    /// collecting one more untested credential. Peeking is how they say
    /// "this answer was never asked for".
    pub(super) fn next_answer(&self) -> Option<String> {
        self.answers.borrow().front().cloned()
    }

    /// Everything the operator saw, joined — the shape the old scripted
    /// console's `transcript()` returned.
    pub(super) fn transcript(&self) -> String {
        self.output.borrow().join("\n")
    }
}
