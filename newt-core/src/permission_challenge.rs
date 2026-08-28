//! Verdict-bound challenges for permission answers.
//!
//! A presence gesture proves a human was there. It does not, by itself, prove
//! *what* they agreed to. If the same challenge were issued for every verdict
//! on a request, an assertion collected while the operator was denying could be
//! replayed as an allow — the signature would verify, because it never covered
//! the verdict.
//!
//! So the challenge binds the request digest **and** the verdict tag. A gesture
//! is then usable for exactly one (request, verdict) pair, and swapping either
//! side produces a challenge the signature does not match.
//!
//! Everything here is pure and wall-clock-free. Expiry is evaluated against a
//! caller-supplied `now`, so the decision is testable and the clock stays
//! injected like everywhere else in the store.

use agent_mesh_protocol::Fingerprint;

use crate::wire_framing::push_field;
use crate::Verdict;

/// Domain tag for the permission-challenge preimage.
const DOMAIN: &[u8] = b"newt/permission-challenge/v1";

/// The stable tag a verdict contributes to a challenge.
///
/// Spelled out rather than derived from `Debug` or a discriminant: a rename or
/// a reordering of the enum must not silently change what a signature covers.
#[must_use]
pub fn verdict_tag(verdict: Verdict) -> &'static [u8] {
    match verdict {
        Verdict::AllowOnce => b"allow_once",
        Verdict::AllowSession => b"allow_session",
        Verdict::Deny => b"deny",
    }
}

/// What a human is being asked to authorize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionChallenge {
    /// The unguessable request id the answer is bound to.
    pub request_id: String,
    /// The conversation the request belongs to.
    pub conversation_id: String,
    /// The serialized requests the operator is looking at.
    pub requests_json: String,
    /// The gate-stamped danger tier.
    pub danger_json: String,
    /// The tick the request was published at.
    pub created_tick: i64,
}

impl PermissionChallenge {
    /// The digest of *what is being decided*, independent of the answer.
    #[must_use]
    pub fn digest(&self) -> Fingerprint {
        let mut payload = Vec::with_capacity(256);
        payload.extend_from_slice(DOMAIN);
        push_field(&mut payload, self.request_id.as_bytes());
        push_field(&mut payload, self.conversation_id.as_bytes());
        push_field(&mut payload, self.requests_json.as_bytes());
        push_field(&mut payload, self.danger_json.as_bytes());
        push_field(&mut payload, &self.created_tick.to_be_bytes());
        Fingerprint::of_bytes(&payload)
    }

    /// The 32 bytes an authenticator signs for one specific verdict.
    ///
    /// `digest ‖ verdict_tag`, framed, then hashed — so neither the request nor
    /// the verdict can be varied without producing a different challenge.
    #[must_use]
    pub fn challenge_for(&self, verdict: Verdict) -> [u8; 32] {
        let mut payload = Vec::with_capacity(128);
        payload.extend_from_slice(DOMAIN);
        push_field(&mut payload, &self.digest().0);
        push_field(&mut payload, verdict_tag(verdict));
        Fingerprint::of_bytes(&payload).0
    }

    /// Whether this request has aged out, per the same TTL the store enforces.
    ///
    /// Checked explicitly at answer time rather than inferred: an assertion
    /// over an expired request must not be honoured just because the signature
    /// is good.
    #[must_use]
    pub fn is_expired(&self, now: i64, ttl_nanos: i64) -> bool {
        now.saturating_sub(self.created_tick) >= ttl_nanos
    }
}

/// Whether a danger tier demands a terminal echo before the answer is honoured.
///
/// High-danger targets get WYSIWYS at the terminal: the un-proxied plain
/// scroller is authoritative about *what* is being authorized, because a
/// compromised browser can render anything it likes next to the button.
#[must_use]
pub fn requires_terminal_echo(danger: &str) -> bool {
    let is_high = |t: &str| t.eq_ignore_ascii_case("high");
    let is_low = |t: &str| t.eq_ignore_ascii_case("low");

    // 1. The form B0b-2 (#1846) writes: a PLAIN tier word.
    let trimmed = danger.trim();
    if is_high(trimmed) {
        return true;
    }
    if is_low(trimmed) {
        return false;
    }
    // 2. The form the gate actually wrote before it — a JSON STRING. This
    //    is #1836: the reader below only ever accepted an ARRAY, so every
    //    real value fell through to the fail-closed default and the
    //    function could not return `false` for any input production
    //    produced, `"low"` included.
    if let Ok(tier) = serde_json::from_str::<String>(danger) {
        if is_high(&tier) {
            return true;
        }
        if is_low(&tier) {
            return false;
        }
    }
    // 3. The form it always claimed to read.
    if let Ok(tiers) = serde_json::from_str::<Vec<String>>(danger) {
        return tiers.iter().any(|t| is_high(t));
    }
    // A tier we cannot parse is treated as high: the failure direction that
    // asks a human rather than the one that skips them.
    true
}
