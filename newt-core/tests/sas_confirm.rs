//! Terminal SAS confirmation and revocation (#1370).
//!
//! Two properties carry this slice. The terminal must derive the words itself,
//! so a staging surface cannot make both screens agree by fiat; and every path
//! that is not an explicit human yes must decline.

use agent_mesh_protocol::UserKey;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use newt_core::credential_registry::{
    append_credential, load_credentials, resolve_credential, revoke_credential,
};
use newt_core::enrollment::EnrollmentCandidate;
use newt_core::sas_confirm::{
    answer_verdict, confirm_enrollment, confirm_prompt, recompute_sas, SasVerdict,
};
use newt_core::sas_transcript::{commit, sas_words, TranscriptInputs};

const ISSUER: &str = "issuer-fp";
const SUBJECT: &str = "operator";

/// A candidate whose claimed transcript genuinely matches its own inputs —
/// what an honest ceremony produces.
fn honest_candidate() -> EnrollmentCandidate {
    let cose_pubkey = b"cose-public-key-bytes".to_vec();
    let enroll_nonce = b"single-use-nonce".to_vec();
    let commitment = commit(&cose_pubkey, b"blinding");
    let transcript = TranscriptInputs {
        rp_id: "newt.example",
        issuer: ISSUER,
        subject: SUBJECT,
        mesh_agent_fingerprint: "agent-fp",
        cose_alg: -7,
        cose_pubkey: &cose_pubkey,
        commitment: &commitment,
        enroll_nonce: &enroll_nonce,
    }
    .transcript_id();

    EnrollmentCandidate {
        credential_id_handle: "cred-abc123".into(),
        cose_pubkey: B64.encode(&cose_pubkey),
        cose_alg: -7,
        mesh_agent_fingerprint: "agent-fp".into(),
        transcript_id: transcript.hex(),
        rp_id: "newt.example".into(),
        commitment: commitment.hex(),
        enroll_nonce: B64.encode(&enroll_nonce),
    }
}

#[test]
fn the_terminal_derives_the_same_words_the_ceremony_did() {
    let candidate = honest_candidate();
    let words = recompute_sas(&candidate, ISSUER, SUBJECT).expect("honest candidate recomputes");

    // Independently: the words must be those of the transcript itself, not of
    // anything the candidate merely asserted.
    let expected = sas_words(&candidate.transcript_id.parse().unwrap());
    assert_eq!(words, expected);
    assert!(confirm_prompt(&words).contains(words[0]));
}

/// The attack the recompute exists to stop: a surface that shows the browser
/// one string and sends a transcript matching a *different* one. Because the
/// terminal rebuilds from the inputs, the lie does not survive.
#[test]
fn a_claimed_transcript_that_does_not_match_its_inputs_is_refused() {
    // A well-formed transcript hex that simply is not this candidate's.
    let mut lying = honest_candidate();
    lying.transcript_id = agent_mesh_protocol::Fingerprint::of_bytes(b"some other ceremony").hex();
    assert!(recompute_sas(&lying, ISSUER, SUBJECT).is_none());

    // Same for a swapped field: the claimed transcript no longer follows.
    let mut swapped = honest_candidate();
    swapped.cose_alg = -8;
    assert!(
        recompute_sas(&swapped, ISSUER, SUBJECT).is_none(),
        "changing a bound field must break the cross-check"
    );

    let mut wrong_rp = honest_candidate();
    wrong_rp.rp_id = "evil.example".into();
    assert!(recompute_sas(&wrong_rp, ISSUER, SUBJECT).is_none());

    // And the issuer/subject the terminal supplies are bound too, so a
    // candidate cannot be replayed under another operator.
    assert!(recompute_sas(&honest_candidate(), "other-issuer", SUBJECT).is_none());
    assert!(recompute_sas(&honest_candidate(), ISSUER, "other-subject").is_none());
}

#[test]
fn malformed_candidate_fields_refuse_rather_than_panic() {
    for mutate in [
        (|c: &mut EnrollmentCandidate| c.cose_pubkey = "not base64!!".into())
            as fn(&mut EnrollmentCandidate),
        |c: &mut EnrollmentCandidate| c.enroll_nonce = "%%%".into(),
        |c: &mut EnrollmentCandidate| c.commitment = "nothex".into(),
    ] {
        let mut candidate = honest_candidate();
        mutate(&mut candidate);
        assert!(recompute_sas(&candidate, ISSUER, SUBJECT).is_none());
    }
}

/// Default-deny: no terminal, no confirmation. A headless run cannot obtain a
/// `PromptWindow` at all, so this is the only outcome available to it.
#[test]
fn a_headless_session_never_confirms() {
    assert_eq!(
        confirm_enrollment(None, &honest_candidate(), ISSUER, SUBJECT),
        SasVerdict::NoTerminal
    );
    assert!(!SasVerdict::NoTerminal.is_confirmed());
    assert!(!SasVerdict::Declined.is_confirmed());
    assert!(!SasVerdict::TranscriptMismatch.is_confirmed());
    assert!(SasVerdict::Confirmed.is_confirmed());
}

/// A mismatched candidate must not even reach a human — there is nothing
/// honest to display, so asking would invite a yes to a fiction. `recompute_sas`
/// returning `None` is exactly the gate `confirm_enrollment` consults before it
/// prompts.
#[test]
fn a_mismatched_candidate_is_refused_before_prompting() {
    let mut lying = honest_candidate();
    lying.mesh_agent_fingerprint = "someone-else".into();
    assert!(
        recompute_sas(&lying, ISSUER, SUBJECT).is_none(),
        "nothing honest to display, so nothing is asked"
    );
}

/// Only an explicit yes promotes. Silence, EOF, and near-misses all decline.
#[test]
fn only_an_explicit_yes_confirms() {
    for yes in ["y", "Y", "yes", "YES", " yes \n", "y\n"] {
        assert_eq!(
            answer_verdict(Some(yes)),
            SasVerdict::Confirmed,
            "{yes:?} should confirm"
        );
    }
    for no in ["", "\n", "n", "no", "yep", "ye", "yes please", "1", "sure"] {
        assert_eq!(
            answer_verdict(Some(no)),
            SasVerdict::Declined,
            "{no:?} must decline"
        );
    }
    assert_eq!(
        answer_verdict(None),
        SasVerdict::Declined,
        "EOF / read error is a refusal, never assent"
    );
}

// --- revocation ---

struct Reg {
    _dir: tempfile::TempDir,
    config: std::path::PathBuf,
    key: UserKey,
}

fn registry_with_two() -> Reg {
    let _dir = tempfile::tempdir().unwrap();
    let config = _dir.path().join("config.toml");
    let key = UserKey::generate();
    for handle in ["cred-abc123", "cred-xyz789"] {
        let mut candidate = honest_candidate();
        candidate.credential_id_handle = handle.into();
        append_credential(&config, SUBJECT, candidate.into_record(1), &key).unwrap();
    }
    Reg { _dir, config, key }
}

#[test]
fn revoking_by_prefix_kills_exactly_one_credential() {
    let reg = registry_with_two();
    let issuer = reg.key.public().fingerprint().hex();

    let full = revoke_credential(&reg.config, SUBJECT, "cred-abc", &reg.key).unwrap();
    assert_eq!(full, "cred-abc123");

    let (registry, warnings) = load_credentials(&reg.config, Some(&reg.key.public()));
    assert!(
        warnings.is_empty(),
        "revocation must stay verifiable: {warnings:?}"
    );
    assert!(
        resolve_credential(&registry, &issuer, SUBJECT, "cred-abc123").is_none(),
        "the revoked credential must not resolve"
    );
    assert!(
        resolve_credential(&registry, &issuer, SUBJECT, "cred-xyz789").is_some(),
        "its sibling must be untouched"
    );
}

/// Revocation re-signs, so the flag cannot simply be edited back out: an
/// unsigned resurrection is dropped fail-closed at load.
#[test]
fn clearing_the_revoked_flag_by_hand_does_not_resurrect_it() {
    let reg = registry_with_two();
    let issuer = reg.key.public().fingerprint().hex();
    revoke_credential(&reg.config, SUBJECT, "cred-abc", &reg.key).unwrap();

    let path = reg
        .config
        .with_file_name("ocap")
        .join("credentials.d")
        .join(format!("{SUBJECT}.toml"));
    let tampered = std::fs::read_to_string(&path)
        .unwrap()
        .replace("revoked = true", "revoked = false");
    std::fs::write(&path, tampered).unwrap();

    let (registry, warnings) = load_credentials(&reg.config, Some(&reg.key.public()));
    assert!(
        resolve_credential(&registry, &issuer, SUBJECT, "cred-abc123").is_none(),
        "un-revoking by hand breaks the signature; the row must stay dead"
    );
    assert!(!warnings.is_empty(), "the tampered row is reported");
}

#[test]
fn revoke_refuses_ambiguity_absence_and_a_foreign_operator() {
    let reg = registry_with_two();

    assert!(
        revoke_credential(&reg.config, SUBJECT, "cred-", &reg.key).is_err(),
        "a prefix matching both must be refused, not guessed"
    );
    assert!(revoke_credential(&reg.config, SUBJECT, "", &reg.key).is_err());
    assert!(revoke_credential(&reg.config, SUBJECT, "nope", &reg.key).is_err());
    assert!(
        revoke_credential(&reg.config, SUBJECT, "cred-abc", &UserKey::generate()).is_err(),
        "another operator must not revoke in this bundle"
    );

    // Revoking twice: the second call finds no *live* match.
    revoke_credential(&reg.config, SUBJECT, "cred-abc", &reg.key).unwrap();
    assert!(revoke_credential(&reg.config, SUBJECT, "cred-abc", &reg.key).is_err());
}
