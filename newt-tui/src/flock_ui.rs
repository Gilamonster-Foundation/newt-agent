//! `/asks` and `/answer`: the operator reads the verified open asks and drafts
//! an answer. Newt composes text and nothing else: the human's y/N and
//! passphrase happen at `amesh flock sign` on their own terminal, so there is
//! no key and no approval path here. Operator input only: a signed brief
//! (flock-origin input) can never run either command.

// The launcher (not yet composed, as in 12b) calls `install` with the signer
// socket and feeds `observe` from the intake's saved boards; until then these
// are exercised by tests only.
#![allow(dead_code)]

use newt_flock::{inert, Answerer, AskView, Outcome};
use std::sync::{Mutex, OnceLock, PoisonError};

const NO_SIGNER: &str =
    "no signer answered: start `amesh flock sign` in another terminal, then run /answer again";

/// The verified ask view and the way to the operator's signer.
pub(crate) struct FlockUi {
    view: Mutex<AskView>,
    answerer: Answerer,
}

static INSTALLED: OnceLock<FlockUi> = OnceLock::new();

/// Register the flock for this process (the explicit setting: the launcher
/// passes the pinned flock and the signer's socket). First call wins.
pub(crate) fn install(ui: FlockUi) {
    let _ = INSTALLED.set(ui);
}

pub(crate) fn installed() -> Option<&'static FlockUi> {
    INSTALLED.get()
}

impl FlockUi {
    pub(crate) fn new(answerer: Answerer) -> Self {
        Self {
            view: Mutex::new(AskView::default()),
            answerer,
        }
    }

    /// Replace the view with the one a verified, saved board shows.
    pub(crate) fn observe(&self, asks: AskView) {
        *self.view.lock().unwrap_or_else(PoisonError::into_inner) = asks;
    }

    fn snapshot(&self) -> AskView {
        self.view
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn list(&self, all: bool) -> Vec<String> {
        let v = self.snapshot();
        let mut out = vec![];
        if v.open.is_empty() {
            out.push("no open asks".to_owned());
        }
        for (i, a) in v.open.iter().enumerate() {
            out.push(format!("{}. {}", i + 1, a.line()));
        }
        if all {
            for (ask, event) in &v.answered {
                out.push(format!("answered {ask} by {event}"));
            }
        }
        out
    }

    fn answer(&self, n: &str, text: &str) -> Vec<String> {
        let v = self.snapshot();
        let Some(ask) = n
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| v.open.get(i))
        else {
            return vec!["usage: /answer <n> <text>  (n is the number from /asks)".to_owned()];
        };
        if text.trim().is_empty() {
            return vec!["usage: /answer <n> <text>  (the answer is empty)".to_owned()];
        }
        let draft = self.answerer.draft(ask, text);
        let (id, shown) = draft.shown();
        let mut out = vec![
            format!("answer to {id}: {}", inert(shown)),
            "waiting for the signer: confirm or decline in the `amesh flock sign` terminal"
                .to_owned(),
        ];
        match self.answerer.send(draft) {
            Err(_) => out.push(NO_SIGNER.to_owned()),
            Ok(Outcome::Refused(why)) => out.push(format!("not signed: {}", inert(&why))),
            Ok(o) => {
                let Outcome::Admitted { event, index } = &o else {
                    return out;
                };
                // the signer's word is not the evidence; only the verified log is
                let seen = o.in_log(&ask.id, &self.snapshot());
                out.push(if seen {
                    format!("answered: {event} is in the verified log at index {index}")
                } else {
                    format!("the signer reports {event} admitted at index {index}; not yet in the verified log")
                });
            }
        }
        out
    }
}

/// Run `/asks` or `/answer` if `line` is one. `None` means "not mine". Input
/// that is not the operator's is never answered, whatever it says.
pub(crate) fn command(operator: bool, line: &str, ui: Option<&FlockUi>) -> Option<Vec<String>> {
    if !operator {
        return None;
    }
    let body = line.strip_prefix('/')?;
    let (name, rest) = body.split_once(char::is_whitespace).unwrap_or((body, ""));
    if name != "asks" && name != "answer" {
        return None;
    }
    let Some(ui) = ui else {
        return Some(vec!["no flock is connected to this session".to_owned()]);
    };
    Some(run(ui, name, rest.trim()))
}

fn run(ui: &FlockUi, name: &str, rest: &str) -> Vec<String> {
    if name == "asks" {
        return ui.list(rest == "all");
    }
    let (n, text) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    ui.answer(n, text.trim())
}
