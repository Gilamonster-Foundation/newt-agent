//! The terminal half of the enrollment ceremony: recompute the short
//! authentication string, show it, and ask a human whether it matches.
//!
//! The ceremony is only worth anything if the two ends derive the string
//! *independently*. If the terminal displayed the `transcript_id` the staging
//! surface sent, a surface that controls both what it shows the browser and
//! what it sends here could make the two agree trivially, and the human's
//! comparison would confirm nothing. So the terminal rebuilds the transcript
//! from the ceremony inputs and derives its own words; the claimed
//! `transcript_id` is used only as a cross-check that must agree.
//!
//! Everything here is default-deny. A [`PromptWindow`] cannot be forged — the
//! only production ways to hold one are
//! [`crate::tty::Terminal::suspend_for_prompt`] and
//! [`crate::tty::Terminal::suspend_for_prompt_to`], plus the test stub under
//! `cfg(test)`. Both production paths share the same sealed suspension helper,
//! so a headless session has no way to reach a confirmation, and
//! [`confirm_enrollment`] returns [`SasVerdict::NoTerminal`] rather than
//! defaulting to yes.

use agent_mesh_protocol::Fingerprint;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;

use crate::enrollment::EnrollmentCandidate;
use crate::sas_transcript::{sas_words, TranscriptInputs, SAS_WORD_COUNT};
use crate::tty::PromptWindow;

/// What the terminal concluded about a staged candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SasVerdict {
    /// A human compared the words and said yes. The only promotable outcome.
    Confirmed,
    /// A human said no, or answered anything that was not an explicit yes.
    Declined,
    /// No terminal was available to ask. Never promotes.
    NoTerminal,
    /// The candidate's own fields do not produce the transcript it claims, so
    /// there is nothing honest to show a human. Never promotes, never prompts.
    TranscriptMismatch,
}

impl SasVerdict {
    /// Whether this verdict permits promotion. Exactly one variant does.
    #[must_use]
    pub fn is_confirmed(self) -> bool {
        matches!(self, Self::Confirmed)
    }
}

/// Rebuild the transcript from the candidate's own inputs and derive the words
/// the terminal should display.
///
/// Returns `None` when the candidate is malformed or when the rebuilt
/// transcript disagrees with the claimed `transcript_id` — both mean the
/// candidate cannot be shown honestly.
#[must_use]
pub fn recompute_sas(
    candidate: &EnrollmentCandidate,
    issuer: &str,
    subject: &str,
) -> Option<[&'static str; SAS_WORD_COUNT]> {
    let cose_pubkey = B64.decode(&candidate.cose_pubkey).ok()?;
    let enroll_nonce = B64.decode(&candidate.enroll_nonce).ok()?;
    let commitment = candidate.commitment.parse::<Fingerprint>().ok()?;

    let transcript = TranscriptInputs {
        rp_id: &candidate.rp_id,
        issuer,
        subject,
        mesh_agent_fingerprint: &candidate.mesh_agent_fingerprint,
        cose_alg: candidate.cose_alg,
        cose_pubkey: &cose_pubkey,
        commitment: &commitment,
        enroll_nonce: &enroll_nonce,
    }
    .transcript_id();

    (transcript.hex() == candidate.transcript_id).then(|| sas_words(&transcript))
}

/// The confirm the operator is shown, as a definition.
///
/// Public so the words it displays and the answers it accepts stay
/// assertable — the two things `confirm_prompt` and `answer_verdict` were
/// tested for before F0c retired them.
///
/// **Deliberately not named `confirm_prompt`.** That is the ratchet's needle
/// for a BESPOKE confirm builder beside the shared path, and this is the
/// opposite: it delegates to `interaction_form::confirm` and adds only this
/// ceremony's question. Taking a baseline row would record a builder that
/// does not exist. (The same call D1b-2 made for `CrewForm::write_confirm`.)
#[must_use]
pub fn confirm_question(words: &[&str; SAS_WORD_COUNT]) -> crate::InteractionDefinition {
    crate::interaction_form::confirm(
        format!("compare code {} matches the browser?", words.join(" ")),
        "",
        "yes, they match",
        "no, they differ",
    )
}

/// Ask the operator whether the browser shows the same words.
///
/// `window` is `Option` so the headless path is representable and lands on
/// [`SasVerdict::NoTerminal`]. It is deliberately not defaulted: a ceremony
/// with nobody watching has not been confirmed by anybody.
///
/// This function is only plumbing — the transcript decision is
/// [`recompute_sas`]'s and the answer decision is
/// `interaction_terminal::confirmed_on_terminal`'s, which resolves through
/// D0's one resolver. The terminal round-trip it wraps is covered at the UAT
/// tier (tmux, both ends side by side), the only tier that can observe a real
/// terminal.
///
/// F0c (#1928) retired this module's private `confirm_prompt` (a `[y/N]`
/// string builder) and `answer_verdict` (a `matches!("y" | "yes")`) — the
/// last inline yes/no parser A0 §9 listed. Two operator-visible consequences,
/// both deliberate:
///
/// * The prompt no longer advertises `[y/N]`. C0c's rule is that a surface
///   never renders a default for a decision, and this decision PROMOTES A
///   KEY — the one place an advertised default is least defensible.
/// * Blank declines rather than being parsed as "not y". Same verdict,
///   arrived at by a rule rather than by falling through a `matches!`.
pub fn confirm_enrollment(
    window: Option<&PromptWindow>,
    candidate: &EnrollmentCandidate,
    issuer: &str,
    subject: &str,
) -> SasVerdict {
    let Some(window) = window else {
        return SasVerdict::NoTerminal;
    };
    let Some(words) = recompute_sas(candidate, issuer, subject) else {
        return SasVerdict::TranscriptMismatch;
    };
    // Echo is derived from the definition — the words are shown, not typed
    // secretly, and no control here is a `Secret`, so it stays `Chars`
    // without this site saying so.
    if crate::interaction_terminal::confirmed_on_terminal(
        window,
        &confirm_question(&words),
        // Blank declines. Nothing about a key promotion may happen because
        // an operator pressed Enter, and EOF is refused by the adapter.
        false,
    ) {
        SasVerdict::Confirmed
    } else {
        SasVerdict::Declined
    }
}
