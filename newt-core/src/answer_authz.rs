//! Who is allowed to answer a permission decision.
//!
//! The pivot of #1366: authority to answer is **key possession**, not a header.
//! `X-Auth-Request-Email` names who is at the browser; it does not prove it,
//! because a forward-auth header is only as trustworthy as everything upstream
//! of it, and it is trivially replayable by anything already inside the network.
//!
//! So once an operator has enrolled a credential, a header-only answer for that
//! operator is **denied outright** rather than downgraded or ignored. The
//! downgrade direction matters: if an unsigned answer merely fell back to
//! header trust, an attacker could strip the assertion and land exactly where
//! enrollment was supposed to stop them. Enrolling must never make you *less*
//! safe than not enrolling.

use crate::Verdict;

/// The outcome of authorizing an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerAuthz {
    /// The assertion verified against an enrolled credential. Honour the
    /// verdict the human actually signed.
    Signed(Verdict),
    /// No credential is enrolled for this operator, so there is nothing to
    /// downgrade *from*; the pre-passkey header path stands.
    Unenrolled(Verdict),
    /// An enrolled operator answered without a usable assertion. This is a hard
    /// deny, not a fallback — see the module note.
    Denied(DenyReason),
}

/// Why an enrolled session's answer was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// Enrolled, but the answer carried no assertion at all.
    MissingAssertion,
    /// The assertion did not verify.
    BadAssertion,
    /// The assertion was for a different request or a different verdict.
    WrongChallenge,
    /// The named credential is not enrolled (or is revoked).
    UnknownCredential,
    /// The request aged out before the answer arrived.
    Expired,
    /// A high-danger target was answered without the terminal echo.
    TerminalEchoRequired,
}

impl DenyReason {
    /// A message safe to show the operator — it must explain the refusal
    /// without narrowing an attacker's search.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::MissingAssertion => "this session is enrolled; answer with your passkey",
            Self::BadAssertion => "the passkey assertion did not verify",
            Self::WrongChallenge => "the assertion does not match this decision",
            Self::UnknownCredential => "that credential is not enrolled",
            Self::Expired => "this decision expired; it must be re-issued",
            Self::TerminalEchoRequired => "confirm this high-danger target at the terminal",
        }
    }
}

impl AnswerAuthz {
    /// The verdict to record, if any. A denial yields `Deny` — never the
    /// verdict that was asked for.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        match self {
            Self::Signed(v) | Self::Unenrolled(v) => *v,
            Self::Denied(_) => Verdict::Deny,
        }
    }

    /// Whether the answer was authorized by key possession.
    #[must_use]
    pub fn is_signed(&self) -> bool {
        matches!(self, Self::Signed(_))
    }
}

/// What the caller knows about the answer being offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnswerContext {
    /// Whether this operator has at least one live enrolled credential.
    pub enrolled: bool,
    /// Whether an assertion accompanied the answer.
    pub assertion_present: bool,
    /// Whether that assertion verified against the stored public key.
    pub assertion_valid: bool,
    /// Whether it was bound to this request and this verdict.
    pub challenge_matches: bool,
    /// Whether the named credential resolved in the registry.
    pub credential_known: bool,
    /// Whether the request has aged out.
    pub expired: bool,
    /// Whether the target is high-danger.
    pub high_danger: bool,
    /// Whether the terminal echoed a high-danger target.
    pub terminal_echoed: bool,
}

/// Decide whether to honour an answer.
///
/// Ordered most-fundamental-first so the reason an operator sees is the most
/// actionable one: an expired request is worth reporting even if the assertion
/// was also missing, because re-signing will not help.
#[must_use]
pub fn authorize(context: AnswerContext, requested: Verdict) -> AnswerAuthz {
    if context.expired {
        return AnswerAuthz::Denied(DenyReason::Expired);
    }
    if !context.enrolled {
        return AnswerAuthz::Unenrolled(requested);
    }
    if !context.assertion_present {
        return AnswerAuthz::Denied(DenyReason::MissingAssertion);
    }
    if !context.credential_known {
        return AnswerAuthz::Denied(DenyReason::UnknownCredential);
    }
    if !context.assertion_valid {
        return AnswerAuthz::Denied(DenyReason::BadAssertion);
    }
    if !context.challenge_matches {
        return AnswerAuthz::Denied(DenyReason::WrongChallenge);
    }
    // Last, because it is the only check a correct, well-signed answer can
    // still fail — and the only one a human can resolve by walking to their
    // terminal rather than re-doing the gesture.
    if context.high_danger && !context.terminal_echoed {
        return AnswerAuthz::Denied(DenyReason::TerminalEchoRequired);
    }
    AnswerAuthz::Signed(requested)
}
