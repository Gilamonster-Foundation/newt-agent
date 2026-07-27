//! Commit-then-reveal enrollment transcript and its short authentication
//! string (SAS).
//!
//! Enrollment binds a session to a WebAuthn credential by having both ends
//! display a short word string and a human confirm they match. The string is
//! only ~66 bits, so the ceremony has to deny an attacker the chance to search
//! for a colliding transcript:
//!
//! 1. The browser commits to the public key it is about to enroll —
//!    [`commit`] over `(cose_pubkey, blinding)` — and sends only the digest.
//! 2. The server then reveals its single-use `enroll_nonce`.
//! 3. Both ends fold the commitment *and* the nonce into the transcript
//!    ([`TranscriptInputs::transcript_id`]) and derive the words from it.
//! 4. At finish the browser reveals `(cose_pubkey, blinding)`, and the server
//!    checks it against the commitment with [`opens`].
//!
//! Because the commitment is fixed before the nonce is known, and the nonce is
//! fixed before the reveal, neither end can grind a key that lands on a chosen
//! word string. Nothing here is secret: every value is a public ceremony input,
//! and the transcript is a hash the human compares. The security comes from the
//! ordering, not from confidentiality.
//!
//! Both ends derive the transcript **independently** — the terminal from what
//! it stored, the browser from what the authenticator handed it. Neither is
//! trusted to send the other a transcript to display.
//!
//! Every function here is pure and wall-clock-free. Nonce and blinding
//! generation belong to the caller (the enrollment channel, slice 4), which
//! keeps this module deterministic and directly comparable against the golden
//! vectors in `tests/data/sas-golden-vectors.json`.

use std::sync::LazyLock;

use agent_mesh_protocol::Fingerprint;

use crate::wire_framing::push_field;

/// Domain tag for the enrollment transcript preimage.
const TRANSCRIPT_DOMAIN: &[u8] = b"newt/passkey-transcript/v1";

/// Domain tag for the commitment preimage. Distinct from the transcript tag so
/// a digest computed for one can never be replayed as the other.
const COMMITMENT_DOMAIN: &[u8] = b"newt/passkey-commitment/v1";

/// The BIP-39 English wordlist, one word per line. See
/// `data/sas-wordlist.LICENSE.txt` for provenance and license.
const WORDLIST: &str = include_str!("../data/sas-wordlist.txt");

/// Words in a short authentication string.
///
/// Six words of [`WORD_BITS`] each carry 66 bits, above the ceremony's 64-bit
/// floor and still short enough to read aloud or compare at a glance.
pub const SAS_WORD_COUNT: usize = 6;

/// Bits consumed per word. The wordlist has exactly `2^11` entries, so every
/// 11-bit value indexes a word and no value is unreachable.
const WORD_BITS: usize = 11;

static WORDS: LazyLock<Vec<&'static str>> = LazyLock::new(|| WORDLIST.lines().collect());

/// The public ceremony inputs both ends fold into the transcript.
///
/// Field order is part of the protocol; the framing makes it unforgeable.
#[derive(Debug, Clone, Copy)]
pub struct TranscriptInputs<'a> {
    /// The relying-party id the assertion will be scoped to.
    pub rp_id: &'a str,
    /// Operator root fingerprint, as stored in the credential registry.
    pub issuer: &'a str,
    /// Operator subject, as stored in the credential registry.
    pub subject: &'a str,
    /// Fingerprint of the mesh agent running the ceremony.
    pub mesh_agent_fingerprint: &'a str,
    /// COSE algorithm identifier (`-7` ES256, `-8` Ed25519).
    pub cose_alg: i64,
    /// Canonical COSE public-key bytes, as returned by the authenticator.
    pub cose_pubkey: &'a [u8],
    /// The browser's commitment from step 1.
    pub commitment: &'a Fingerprint,
    /// The server's single-use enrollment nonce, revealed in step 2.
    pub enroll_nonce: &'a [u8],
}

impl TranscriptInputs<'_> {
    /// BLAKE3 over the framed transcript preimage.
    ///
    /// This is the value stored as `transcript_id` on a credential record, and
    /// the value [`sas_words`] reduces to a comparable string.
    #[must_use]
    pub fn transcript_id(&self) -> Fingerprint {
        let mut payload = Vec::with_capacity(256);
        payload.extend_from_slice(TRANSCRIPT_DOMAIN);
        push_field(&mut payload, self.rp_id.as_bytes());
        push_field(&mut payload, self.issuer.as_bytes());
        push_field(&mut payload, self.subject.as_bytes());
        push_field(&mut payload, self.mesh_agent_fingerprint.as_bytes());
        push_field(&mut payload, &self.cose_alg.to_be_bytes());
        push_field(&mut payload, self.cose_pubkey);
        push_field(&mut payload, &self.commitment.0);
        push_field(&mut payload, self.enroll_nonce);
        Fingerprint::of_bytes(&payload)
    }
}

/// Commit to a public key before the server reveals its nonce.
///
/// `blinding` must be fresh random bytes. Without it the commitment would be a
/// hash of a value an attacker can guess — authenticator public keys are not
/// secret — and revealing it early would defeat the ordering the ceremony
/// depends on.
#[must_use]
pub fn commit(cose_pubkey: &[u8], blinding: &[u8]) -> Fingerprint {
    let mut payload = Vec::with_capacity(128);
    payload.extend_from_slice(COMMITMENT_DOMAIN);
    push_field(&mut payload, cose_pubkey);
    push_field(&mut payload, blinding);
    Fingerprint::of_bytes(&payload)
}

/// Whether a revealed `(cose_pubkey, blinding)` opens `commitment`.
///
/// Compared in constant time: the reveal arrives from the network, and a
/// byte-at-a-time comparison would leak how much of a forged opening was
/// correct.
#[must_use]
pub fn opens(commitment: &Fingerprint, cose_pubkey: &[u8], blinding: &[u8]) -> bool {
    let computed = commit(cose_pubkey, blinding);
    let mut diff = 0u8;
    for (a, b) in computed.0.iter().zip(commitment.0.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Reduce a transcript to the words both ends display.
///
/// Bits are taken from the front of the digest, most-significant first, eleven
/// at a time. Stated exactly so a port can reproduce it: word `i` is the
/// integer formed by transcript bits `11*i .. 11*i+11`, where bit 0 is the
/// high bit of byte 0.
#[must_use]
pub fn sas_words(transcript: &Fingerprint) -> [&'static str; SAS_WORD_COUNT] {
    std::array::from_fn(|i| WORDS[word_index(&transcript.0, i)])
}

fn word_index(digest: &[u8; 32], word: usize) -> usize {
    let start = word * WORD_BITS;
    (start..start + WORD_BITS).fold(0, |value, bit| {
        let set = digest[bit / 8] & (0x80 >> (bit % 8)) != 0;
        (value << 1) | usize::from(set)
    })
}
