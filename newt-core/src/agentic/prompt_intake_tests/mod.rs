//! Test families for [`super`] (`agentic::prompt_intake`), split out of
//! `prompt_intake.rs` by what each test asserts. Sibling files; no behaviour
//! change.
//!
//! `GIT_REMOTE_FYI` and `LARGEST_FILES_PROMPT` live here because each is read
//! by more than one family. `the_evidence_prompt_is_the_recorded_length` is
//! the fixture-integrity twin for the first of them, so it stays beside it.

// A glob RE-EXPORT, not a plain glob: the moved bodies write `super::X` from
// when `super` was `prompt_intake`, and a private glob binding is not
// nameable by path from a child module.
pub(crate) use super::*;

// -----------------------------------------------------------------
// #1971 — an informational prompt grants no act authority, and the fact
// it states survives the turn.
// -----------------------------------------------------------------

/// The evidenced prompt, reconstructed to its recorded shape and EXACT
/// recorded length.
///
/// The artifact that evidenced this bug (`kind=decision`, `body NULL`)
/// stored `atomic_ask_count=1` and a digest — so the text is not
/// recoverable from it, and the conversation it came from is absent from
/// the surviving `conversations.db`. What the issue does record is the
/// shape (a git-remote statement of fact, no imperative) and the length
/// (92 bytes, `wc -c` verified). This is that shape at that length.
///
/// **That the input cannot be recovered from its own durable record is
/// itself the second half of this bug**, and is why
/// `artifact_metadata` now carries informational text rather than only a
/// digest.
const GIT_REMOTE_FYI: &str =
    "the git remote for agent-voice repo is git@github.com:Gilamonster-Foundation/agent-voice.git";

#[test]
fn the_evidence_prompt_is_the_recorded_length() {
    assert_eq!(
        GIT_REMOTE_FYI.len(),
        92,
        "the issue records 92 bytes (wc -c verified); a reconstruction of a \
         different length is not the case being pinned"
    );
}

/// The #1257 canonical prompt. Today's defaults classify it Research by
/// CONTENT ("largest" is evidence-phrasing data) — not the `?` cliff.
const LARGEST_FILES_PROMPT: &str = "What are the 10 largest Rust files in this workspace?";

#[cfg(test)]
#[path = "atomic_ask.rs"]
mod atomic_ask;
#[cfg(test)]
#[path = "card_and_artifact.rs"]
mod card_and_artifact;
#[cfg(test)]
#[path = "clarification_gate.rs"]
mod clarification_gate;
#[cfg(test)]
#[path = "disposition_inference.rs"]
mod disposition_inference;
#[cfg(test)]
#[path = "imperative_recognition.rs"]
mod imperative_recognition;
#[cfg(test)]
#[path = "informational_authority.rs"]
mod informational_authority;
#[cfg(test)]
#[path = "negation.rs"]
mod negation;
