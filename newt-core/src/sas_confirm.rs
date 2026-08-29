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
//! only ways to hold one are [`crate::tty::Terminal::suspend_for_prompt`] and
//! the test stub — so a headless session has no way to reach a confirmation,
//! and [`confirm_enrollment`] returns [`SasVerdict::NoTerminal`] rather than
//! defaulting to yes.

use agent_mesh_protocol::Fingerprint;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;

use crate::enrollment::EnrollmentCandidate;
use crate::sas_transcript::{sas_words, TranscriptInputs, SAS_WORD_COUNT};
use crate::tty::{read_prompt_window_line, Echo, PromptLine, PromptWindow};

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

/// The prompt a human answers, given the words the terminal derived itself.
#[must_use]
pub fn confirm_prompt(words: &[&str; SAS_WORD_COUNT]) -> String {
    format!(
        "compare code {} matches the browser? [y/N] ",
        words.join(" ")
    )
}

/// The verdict for what a human typed.
///
/// `None` means there was no answer at all — EOF (Ctrl-D) or a read error.
/// Both decline: silence is not assent. Only an explicit yes is a yes; blank,
/// "n", a stray keypress, and a pasted line all decline.
///
/// Split out from [`confirm_enrollment`] so the decision is testable without a
/// [`PromptWindow`]. The capability cannot be constructed outside `tty` — that
/// seal is itself compile-fail-tested — so anything that must be proven by a
/// unit test has to live on this side of the boundary.
#[must_use]
pub fn answer_verdict(answer: Option<&str>) -> SasVerdict {
    match answer {
        Some(text) if matches!(text.trim().to_ascii_lowercase().as_str(), "y" | "yes") => {
            SasVerdict::Confirmed
        }
        _ => SasVerdict::Declined,
    }
}

/// Ask the operator whether the browser shows the same words.
///
/// `window` is `Option` so the headless path is representable and lands on
/// [`SasVerdict::NoTerminal`]. It is deliberately not defaulted: a ceremony
/// with nobody watching has not been confirmed by anybody.
///
/// This function is only plumbing — every decision it makes is delegated to
/// [`recompute_sas`] and [`answer_verdict`]. The terminal round-trip it wraps
/// is covered at the UAT tier (tmux, both ends side by side), which is the only
/// tier that can observe a real terminal.
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
    // The SAS words are shown, not typed secretly — an ordinary echo.
    match read_prompt_window_line(window, &confirm_prompt(&words), Echo::Chars) {
        Ok(PromptLine::Line(answer)) => answer_verdict(Some(&answer)),
        Ok(PromptLine::Eof | PromptLine::Back | PromptLine::Exit) | Err(_) => SasVerdict::Declined,
    }
}
