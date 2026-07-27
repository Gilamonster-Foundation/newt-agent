//! Cross-port and tamper-binding laws for the passkey enrollment transcript.
//!
//! The vectors in `data/sas-golden-vectors.json` were produced by an
//! implementation written from the spec in `newt_core::sas_transcript`'s module
//! docs rather than from its source, so these tests check the spec is portable,
//! not merely that the Rust agrees with itself. The browser JS port (slice 7)
//! must reproduce the same file.

use std::collections::HashSet;
use std::str::FromStr;

use agent_mesh_protocol::Fingerprint;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use newt_core::sas_transcript::{commit, opens, sas_words, TranscriptInputs, SAS_WORD_COUNT};
use serde_json::Value;

const WORDLIST: &str = include_str!("../data/sas-wordlist.txt");
const VECTORS: &str = include_str!("data/sas-golden-vectors.json");

fn vectors() -> Value {
    serde_json::from_str(VECTORS).expect("golden vectors parse")
}

fn b64(case: &Value, field: &str) -> Vec<u8> {
    B64.decode(case[field].as_str().expect("base64 field"))
        .expect("base64 decodes")
}

fn digest(case: &Value, field: &str) -> Fingerprint {
    Fingerprint::from_str(case[field].as_str().expect("hex field")).expect("hex digest parses")
}

fn words(case: &Value) -> Vec<String> {
    case["sas_words"]
        .as_array()
        .expect("sas_words array")
        .iter()
        .map(|w| w.as_str().expect("word").to_owned())
        .collect()
}

#[test]
fn golden_vectors_match_an_independent_implementation() {
    let doc = vectors();
    let cases = doc["cases"].as_array().expect("cases array");
    assert!(!cases.is_empty(), "vectors file must carry cases");

    for case in cases {
        let name = case["name"].as_str().expect("case name");
        let cose_pubkey = b64(case, "cose_pubkey_b64");
        let blinding = b64(case, "blinding_b64");
        let enroll_nonce = b64(case, "enroll_nonce_b64");

        let commitment = commit(&cose_pubkey, &blinding);
        assert_eq!(
            commitment,
            digest(case, "commitment_hex"),
            "{name}: commitment drifted from the pinned vector"
        );
        assert!(
            opens(&commitment, &cose_pubkey, &blinding),
            "{name}: the true reveal must open its own commitment"
        );

        let inputs = TranscriptInputs {
            rp_id: case["rp_id"].as_str().expect("rp_id"),
            issuer: case["issuer"].as_str().expect("issuer"),
            subject: case["subject"].as_str().expect("subject"),
            mesh_agent_fingerprint: case["mesh_agent_fingerprint"]
                .as_str()
                .expect("mesh_agent_fingerprint"),
            cose_alg: case["cose_alg"].as_i64().expect("cose_alg"),
            cose_pubkey: &cose_pubkey,
            commitment: &commitment,
            enroll_nonce: &enroll_nonce,
        };
        let transcript = inputs.transcript_id();
        assert_eq!(
            transcript,
            digest(case, "transcript_hex"),
            "{name}: transcript drifted from the pinned vector"
        );
        assert_eq!(
            sas_words(&transcript).to_vec(),
            words(case),
            "{name}: SAS drifted from the pinned vector"
        );
    }
}

#[test]
fn sas_vectors_pin_the_bit_extraction_order() {
    let doc = vectors();
    for case in doc["sas_cases"].as_array().expect("sas_cases array") {
        let name = case["name"].as_str().expect("case name");
        let transcript = digest(case, "transcript_hex");
        assert_eq!(
            sas_words(&transcript).to_vec(),
            words(case),
            "{name}: bit extraction drifted"
        );
    }
}

/// The wordlist is load-bearing twice over: an 11-bit index is only total
/// because there are exactly 2048 entries, and a port that ships a different
/// list produces a different string for the same transcript while looking
/// correct. Both ends of a ceremony must agree on this file byte-for-byte.
#[test]
fn wordlist_is_the_pinned_two_thousand_forty_eight_entries() {
    let doc = vectors();
    assert_eq!(
        blake3::hash(WORDLIST.as_bytes()).to_hex().as_str(),
        doc["wordlist_blake3"].as_str().expect("wordlist_blake3"),
        "the wordlist changed; every existing SAS and port is invalidated"
    );

    let words: Vec<&str> = WORDLIST.lines().collect();
    assert_eq!(words.len(), 1 << 11, "11 bits must index every entry");
    assert_eq!(
        words.iter().collect::<HashSet<_>>().len(),
        words.len(),
        "a repeated word makes two transcripts display identically"
    );
    assert_eq!(
        words
            .iter()
            .map(|w| &w[..w.len().min(4)])
            .collect::<HashSet<_>>()
            .len(),
        words.len(),
        "four-character prefixes must stay unique for transcription"
    );
    assert!(
        words
            .iter()
            .all(|w| w.chars().all(|c| c.is_ascii_lowercase())),
        "non-ascii-lowercase words break terminal and browser rendering alike"
    );
    assert_eq!(doc["word_bits"].as_u64(), Some(11));
    assert_eq!(doc["sas_word_count"].as_u64(), Some(SAS_WORD_COUNT as u64));
}

#[test]
fn opens_rejects_every_reveal_but_the_true_one() {
    let pubkey = b"cose-public-key";
    let blinding = b"blinding-bytes!";
    let commitment = commit(pubkey, blinding);

    assert!(opens(&commitment, pubkey, blinding));
    assert!(
        !opens(&commitment, b"cose-public-keY", blinding),
        "a substituted key must not open the commitment"
    );
    assert!(
        !opens(&commitment, pubkey, b"blinding-bytes?"),
        "a substituted blinding must not open the commitment"
    );
    assert!(
        !opens(&commitment, blinding, pubkey),
        "equal-length arguments must not be interchangeable"
    );
    assert!(
        !opens(&commit(pubkey, b"other"), pubkey, blinding),
        "a foreign commitment must not be opened"
    );
}

/// Nine variants, each differing from the base in exactly one field, plus a
/// pair that shifts a byte across the issuer/subject boundary. All must hash
/// apart: a transcript that ignores a field lets a MITM vary it freely while
/// the human sees the same words.
#[test]
fn every_transcript_field_is_bound() {
    let pubkey = b"pk";
    let alt_pubkey = b"pq";
    let commitment = commit(pubkey, b"blind");
    let alt_commitment = commit(alt_pubkey, b"blind");
    let base = TranscriptInputs {
        rp_id: "newt.example",
        issuer: "issuer",
        subject: "subject",
        mesh_agent_fingerprint: "agent-fp",
        cose_alg: -7,
        cose_pubkey: pubkey,
        commitment: &commitment,
        enroll_nonce: b"nonce",
    };

    let variants = [
        base,
        TranscriptInputs {
            rp_id: "evil.example",
            ..base
        },
        TranscriptInputs {
            issuer: "issuen",
            ..base
        },
        TranscriptInputs {
            subject: "subjecu",
            ..base
        },
        TranscriptInputs {
            mesh_agent_fingerprint: "agent-fq",
            ..base
        },
        TranscriptInputs {
            cose_alg: -8,
            ..base
        },
        TranscriptInputs {
            cose_pubkey: alt_pubkey,
            ..base
        },
        TranscriptInputs {
            commitment: &alt_commitment,
            ..base
        },
        TranscriptInputs {
            enroll_nonce: b"nonch",
            ..base
        },
        // Same concatenation, different split: framing must separate them.
        TranscriptInputs {
            issuer: "issuers",
            subject: "ubject",
            ..base
        },
    ];

    let distinct: HashSet<_> = variants
        .iter()
        .map(TranscriptInputs::transcript_id)
        .collect();
    assert_eq!(
        distinct.len(),
        variants.len(),
        "two distinct ceremonies produced one transcript"
    );

    let sas: HashSet<_> = variants
        .iter()
        .map(|v| sas_words(&v.transcript_id()))
        .collect();
    assert_eq!(
        sas.len(),
        variants.len(),
        "distinct transcripts collided in the displayed words"
    );
}
