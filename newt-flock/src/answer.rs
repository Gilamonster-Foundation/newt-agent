//! Sending a draft answer to the operator's separate signer.
//!
//! Newt composes text and nothing else. The signer (`amesh flock sign`) runs on
//! the operator's own terminal, shows the exact answer, and asks the human; this
//! module holds no key, never confirms for anyone, and offers no way for a model
//! to approve anything: a [`Draft`] is sent only by the code that owns the
//! human's keystroke.

use crate::asks::OpenAsk;
use agent_mesh_flock_server::wire::{read_msg, write_msg};
use content_addressable::ContentId;
use serde::{Deserialize, Serialize};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// The signer's request. Field for field the format of
/// `agent_mesh_cli::operator::SignRequest` (a test compares golden bytes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignRequest {
    /// The flock.
    pub flock: ContentId,
    /// The ask answered.
    pub ask: ContentId,
    /// The answer text.
    pub text: String,
}

/// The signer's reply (same contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignReply {
    /// Admitted.
    Admitted {
        /// The answer's id.
        event: ContentId,
        /// Its admission index.
        index: u64,
    },
    /// Nothing was signed.
    Refused(String),
}

/// What came of a sent draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The signer says the server admitted this event. Confirm it against the
    /// verified log with [`Outcome::in_log`] before showing it as done.
    Admitted {
        /// The answer event.
        event: ContentId,
        /// Its admission index.
        index: u64,
    },
    /// Nothing was signed; the signer's reason.
    Refused(String),
}

impl Outcome {
    /// Whether the verified log agrees: `ask` is closed by exactly the event the
    /// signer reported. The signer's word is not the evidence; the log is.
    #[must_use]
    pub fn in_log(&self, ask: &ContentId, view: &crate::AskView) -> bool {
        matches!(self, Self::Admitted { event, .. } if view.answered.get(ask) == Some(event))
    }
}

/// An answer being composed. It is immutable once made: what [`Draft::shown`]
/// returns is what [`Answerer::send`] puts on the wire, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    ask: ContentId,
    text: String,
}

impl Draft {
    /// The ask and the exact text the human is looking at.
    #[must_use]
    pub fn shown(&self) -> (&ContentId, &str) {
        (&self.ask, &self.text)
    }

    /// Throw the draft away. Nothing was sent, so nothing is undone.
    pub fn cancel(self) {}
}

/// Talks to one operator signer about one flock.
pub struct Answerer {
    flock: ContentId,
    signer: PathBuf,
}

impl Answerer {
    /// For the pinned flock and the signer's socket.
    #[must_use]
    pub fn new(flock: ContentId, signer: PathBuf) -> Self {
        Self { flock, signer }
    }

    /// A draft answer to a verified open ask.
    #[must_use]
    pub fn draft(&self, ask: &OpenAsk, text: &str) -> Draft {
        Draft {
            ask: ask.id,
            text: text.to_owned(),
        }
    }

    /// Send a draft to the signer and wait for the human's decision there.
    /// Consumes the draft: a refused or declined answer is composed again, not
    /// retried blindly.
    ///
    /// # Errors
    /// The signer is not running or the connection broke; the draft's fate is
    /// then unknown until the log says otherwise.
    pub fn send(&self, draft: Draft) -> Result<Outcome, std::io::Error> {
        let mut c = UnixStream::connect(&self.signer)?;
        let req = SignRequest {
            flock: self.flock,
            ask: draft.ask,
            text: draft.text,
        };
        write_msg(&mut c, &req).map_err(io)?;
        match read_msg::<SignReply>(&mut c).map_err(io)? {
            Some(SignReply::Admitted { event, index }) => Ok(Outcome::Admitted { event, index }),
            Some(SignReply::Refused(why)) => Ok(Outcome::Refused(why)),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the signer closed without an answer",
            )),
        }
    }
}

fn io(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}
