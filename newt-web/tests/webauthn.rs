//! Relying-party binding checks (#1371).
//!
//! These are the checks agent-bridle's verifiers do **not** perform. A valid
//! signature made for another site is still a valid signature, so the suite is
//! mostly about refusing proofs that would otherwise verify fine.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use newt_web::webauthn::{
    client_data_challenge, parse_authenticator_data, CoseAlg, RelyingParty, RpError,
};
use sha2::{Digest, Sha256};

const RP_ID: &str = "newt.example";
const ORIGIN: &str = "https://newt.example:8443";

fn rp() -> RelyingParty {
    RelyingParty::new(RP_ID, ORIGIN).expect("configured")
}

fn client_data(origin: &str) -> Vec<u8> {
    format!(
        r#"{{"type":"webauthn.get","challenge":"{}","origin":"{origin}"}}"#,
        B64URL.encode([7u8; 32])
    )
    .into_bytes()
}

/// `rpIdHash(32) ‖ flags ‖ signCount(4)`, optionally followed by attested
/// credential data.
fn authenticator_data(rp_id: &str, with_credential: bool) -> Vec<u8> {
    let mut data = Sha256::digest(rp_id.as_bytes()).to_vec();
    data.push(if with_credential {
        0b0100_0101
    } else {
        0b0000_0101
    });
    data.extend_from_slice(&0u32.to_be_bytes());
    if with_credential {
        data.extend_from_slice(&[0u8; 16]); // aaguid
        let cred_id = b"credential-id";
        data.extend_from_slice(&(cred_id.len() as u16).to_be_bytes());
        data.extend_from_slice(cred_id);
        // Minimal COSE map carrying only what we read: label 3 = alg = -7.
        let mut cose = Vec::new();
        ciborium::into_writer(
            &ciborium::value::Value::Map(vec![(
                ciborium::value::Value::Integer(3.into()),
                ciborium::value::Value::Integer((-7).into()),
            )]),
            &mut cose,
        )
        .unwrap();
        data.extend_from_slice(&cose);
    }
    data
}

// --- fail-closed configuration ---

/// No env, no relying party — and therefore no way to verify anything. The
/// deployment cannot silently run with origin checking off.
#[test]
fn an_unconfigured_relying_party_cannot_be_built() {
    assert_eq!(
        RelyingParty::new("", ORIGIN).unwrap_err(),
        RpError::NotConfigured("NEWT_WEB_RP_ID")
    );
    assert_eq!(
        RelyingParty::new(RP_ID, "").unwrap_err(),
        RpError::NotConfigured("NEWT_WEB_ORIGIN")
    );
    assert_eq!(
        RelyingParty::new("   ", ORIGIN).unwrap_err(),
        RpError::NotConfigured("NEWT_WEB_RP_ID"),
        "whitespace is not configuration"
    );
}

// --- origin ---

/// Exact match only. A suffix or prefix test is the classic way this check is
/// written wrong, and each of these would pass one.
#[test]
fn origin_must_match_exactly() {
    let rp = rp();
    assert!(rp.check_origin(&client_data(ORIGIN)).is_ok());

    for hostile in [
        "https://newt.example",           // right host, wrong port
        "https://newt.example:8444",      // wrong port
        "http://newt.example:8443",       // wrong scheme
        "https://newt.example.evil.test", // suffix attack
        "https://evil.test/newt.example", // path, not origin
        "https://NEWT.example:8443",      // case
        "https://newt.example:8443/",     // trailing slash is a different origin
    ] {
        let err = rp.check_origin(&client_data(hostile)).unwrap_err();
        assert!(
            matches!(err, RpError::OriginMismatch { .. }),
            "{hostile} must be refused, got {err:?}"
        );
    }
}

#[test]
fn malformed_client_data_is_refused() {
    let rp = rp();
    for bad in [
        &b"not json"[..],
        &b"{}"[..],
        br#"{"origin":42}"#,
        &[0xff, 0xfe][..], // not utf-8
    ] {
        assert_eq!(
            rp.check_origin(bad).unwrap_err(),
            RpError::MalformedClientData
        );
    }
}

// --- rpIdHash ---

#[test]
fn rp_id_hash_must_match_and_must_be_present() {
    let rp = rp();
    assert!(rp
        .check_rp_id_hash(&authenticator_data(RP_ID, false))
        .is_ok());

    assert_eq!(
        rp.check_rp_id_hash(&authenticator_data("evil.example", false))
            .unwrap_err(),
        RpError::RpIdHashMismatch
    );
    assert_eq!(
        rp.check_rp_id_hash(&[0u8; 16]).unwrap_err(),
        RpError::RpIdHashMismatch,
        "a truncated authenticatorData must refuse, not index out of bounds"
    );
    assert_eq!(
        rp.check_rp_id_hash(&[]).unwrap_err(),
        RpError::RpIdHashMismatch
    );
}

/// Both bindings are required together: an assertion missing either half is
/// not a WebAuthn proof and must not reach a signature check.
#[test]
fn both_bindings_are_required() {
    let rp = rp();
    let cd = client_data(ORIGIN);
    let ad = authenticator_data(RP_ID, false);

    assert!(rp.check_bindings(Some(&cd), Some(&ad)).is_ok());
    assert_eq!(
        rp.check_bindings(None, Some(&ad)).unwrap_err(),
        RpError::NotWebAuthn
    );
    assert_eq!(
        rp.check_bindings(Some(&cd), None).unwrap_err(),
        RpError::NotWebAuthn
    );
    assert_eq!(
        rp.check_bindings(None, None).unwrap_err(),
        RpError::NotWebAuthn
    );

    // A good origin does not excuse a bad rpIdHash, and vice versa.
    let wrong_ad = authenticator_data("evil.example", false);
    assert_eq!(
        rp.check_bindings(Some(&cd), Some(&wrong_ad)).unwrap_err(),
        RpError::RpIdHashMismatch
    );
    let wrong_cd = client_data("https://evil.test");
    assert!(matches!(
        rp.check_bindings(Some(&wrong_cd), Some(&ad)).unwrap_err(),
        RpError::OriginMismatch { .. }
    ));
}

// --- algorithm dispatch ---

/// Exhaustive with an explicit deny arm: a COSE identifier we have not vetted
/// must not widen what we accept just by appearing on the wire.
#[test]
fn only_vetted_algorithms_are_accepted() {
    assert_eq!(CoseAlg::from_cose(-7).unwrap(), CoseAlg::Es256);
    assert_eq!(CoseAlg::from_cose(-8).unwrap(), CoseAlg::Ed25519);
    assert_eq!(CoseAlg::Es256.as_cose(), -7);
    assert_eq!(CoseAlg::Ed25519.as_cose(), -8);

    for hostile in [-257i64, -65535, 0, 1, -6, -9, i64::MIN, i64::MAX] {
        assert_eq!(
            CoseAlg::from_cose(hostile).unwrap_err(),
            RpError::UnsupportedAlgorithm(hostile),
            "COSE {hostile} must be refused"
        );
    }
}

// --- attestation parsing ---

#[test]
fn attested_credential_data_yields_the_key_and_algorithm() {
    let parsed = parse_authenticator_data(&authenticator_data(RP_ID, true)).expect("parses");
    assert_eq!(parsed.credential_id, b"credential-id");
    assert_eq!(parsed.alg, CoseAlg::Es256);
    assert!(!parsed.cose_pubkey.is_empty());
}

#[test]
fn attestation_parsing_refuses_truncation_and_missing_credential_data() {
    // Flag bit clear = no attested credential data to extract.
    assert_eq!(
        parse_authenticator_data(&authenticator_data(RP_ID, false)).unwrap_err(),
        RpError::MalformedAttestation
    );
    // Truncated at every length up to a full header: none may panic.
    let full = authenticator_data(RP_ID, true);
    for cut in 0..full.len().min(60) {
        assert!(parse_authenticator_data(&full[..cut]).is_err());
    }
}

// --- challenge extraction ---

#[test]
fn the_client_data_challenge_round_trips() {
    assert_eq!(
        client_data_challenge(&client_data(ORIGIN)).unwrap(),
        [7u8; 32]
    );
    assert_eq!(
        client_data_challenge(br#"{"origin":"x"}"#).unwrap_err(),
        RpError::MalformedClientData
    );
    assert_eq!(
        client_data_challenge(br#"{"challenge":"!!!not base64!!!"}"#).unwrap_err(),
        RpError::MalformedClientData
    );
}
