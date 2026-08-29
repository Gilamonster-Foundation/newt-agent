//! Whether a danger tier demands a terminal echo before an answer is honoured.
//!
//! # What used to live here, and why it is gone (#1839)
//!
//! This module was `PermissionChallenge`: a hand-rolled canonical encoding
//! that hashed a request's fields into a `Fingerprint`, then bound that digest
//! to a verdict tag so a gesture collected while the operator was denying
//! could not be replayed as an allow.
//!
//! The property still matters; the bespoke encoding does not. A2 gave every
//! interaction a `DefinitionId` — a `ContentId` over the canonical dag-cbor of
//! the whole record — so "what is being decided" already has an identity that
//! is minted once, by the `content-addressable` crate, instead of assembled
//! here out of length-prefixed fields. A second hand-rolled canonicalization
//! beside it is the defect the first-principle rule names, not a feature.
//!
//! `PermissionChallenge` had **zero production callers** — re-verified across
//! every crate before deletion: its only references were this crate's module
//! declaration and its own test file. So `digest()`, `challenge_for()`,
//! `is_expired()`, `verdict_tag()`, the struct, and
//! `newt-core/tests/permission_challenge.rs` are deleted outright rather than
//! carried as a compatibility arm.
//!
//! **`wire_framing` stays.** #1837's deletion gate paired the two and called
//! them both callerless; that premise was false, and re-verified as false at
//! `58afe828`. `wire_framing::push_field` has three production callers with 28
//! call sites between them — `sas_transcript`, `dock_registry`,
//! `credential_registry`, all security-relevant registries whose on-disk
//! encodings it defines. Deleting it would have broken all three.
//!
//! # What the deletion leaves behind, stated rather than left to be found
//!
//! `PermissionChallenge` was the Rust half of the #1366/#1373 passkey-answer
//! path, which was **designed but never wired** — a fact the A0 inventory
//! already recorded independently
//! (`docs/findings/2026-08-newt-markup-a0-inventory.md:570`: "`answer_authz` +
//! `permission_challenge` are library-only — consumed by nothing in
//! production … designed but unwired"). Its browser half is unwired too:
//! `newt-web/assets/webauthn.js`'s `signVerdict` reads a `data-challenge`
//! attribute that nothing renders, and `newtSignVerdict` has no caller outside
//! a test asserting the string appears in the served asset.
//!
//! So this deletes dead code, not a live check. But it does leave a comment in
//! `webauthn.js:142` describing a server behaviour ("the server binds
//! digest+verdict_tag before issuing it") that now has no implementation at
//! all. That residue belongs with #1839 part 2 — the other unwired half of the
//! same design, which B0 owns — and is deliberately NOT patched here:
//! `newt-web` is another slice's territory, and re-minting the binding is
//! B0's call to make against the definition's `ContentId` rather than a
//! resurrection of this encoding.
//!
//! What remains here is [`requires_terminal_echo`], which is live, is on the
//! authorization path, and was fixed under #1836.

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
