//! Enrollment route, CSP, and SRI (#1372).
//!
//! The load-bearing test here is `both_ends_derive_the_same_words`: the browser
//! and the terminal must independently arrive at the same six words, and this
//! is the only place in the suite where both derivations run side by side.
//! Everything else is refusal behaviour and header hygiene.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use newt_web::csp::{policy, sri, Nonce, HTMX_JS, WEBAUTHN_JS};
use newt_web::enroll::{prepare, render_page, EnrollmentContext, FinishError, FinishRequest};
use newt_web::webauthn::RelyingParty;
use sha2::{Digest, Sha256};

const RP_ID: &str = "newt.example";
const ORIGIN: &str = "https://newt.example:8443";
const ISSUER: &str = "9f2c1a4b8d6e";

fn rp() -> RelyingParty {
    RelyingParty::new(RP_ID, ORIGIN).unwrap()
}

fn context() -> EnrollmentContext {
    EnrollmentContext {
        issuer: ISSUER.into(),
        subject: "operator".into(),
    }
}

/// A CBOR attestation object wrapping authenticator data for `rp_id`.
fn attestation(rp_id: &str) -> Vec<u8> {
    let mut auth = Sha256::digest(rp_id.as_bytes()).to_vec();
    auth.push(0b0100_0101);
    auth.extend_from_slice(&0u32.to_be_bytes());
    auth.extend_from_slice(&[0u8; 16]);
    let cred = b"cred-id";
    auth.extend_from_slice(&(cred.len() as u16).to_be_bytes());
    auth.extend_from_slice(cred);
    let mut cose = Vec::new();
    ciborium::into_writer(
        &ciborium::value::Value::Map(vec![(
            ciborium::value::Value::Integer(3.into()),
            ciborium::value::Value::Integer((-7).into()),
        )]),
        &mut cose,
    )
    .unwrap();
    auth.extend_from_slice(&cose);

    let mut out = Vec::new();
    ciborium::into_writer(
        &ciborium::value::Value::Map(vec![
            (
                ciborium::value::Value::Text("fmt".into()),
                ciborium::value::Value::Text("none".into()),
            ),
            (
                ciborium::value::Value::Text("authData".into()),
                ciborium::value::Value::Bytes(auth),
            ),
        ]),
        &mut out,
    )
    .unwrap();
    out
}

fn request(origin: &str, rp_id: &str) -> FinishRequest {
    let client_data =
        format!(r#"{{"type":"webauthn.create","challenge":"AAAA","origin":"{origin}"}}"#);
    FinishRequest {
        credential_id: B64URL.encode(b"cred-id"),
        attestation_object: B64URL.encode(attestation(rp_id)),
        client_data_json: B64URL.encode(client_data.as_bytes()),
        blinding: B64URL.encode([9u8; 32]),
    }
}

/// The ceremony's whole point: the words the browser is told to show and the
/// words the terminal derives from the staged candidate must be identical.
/// If this drifts, every enrollment fails in the operator's hands with no
/// server-side signal at all.
#[test]
fn both_ends_derive_the_same_words() {
    let (candidate, response) = prepare(
        Some(&rp()),
        Some(&context()),
        &request(ORIGIN, RP_ID),
        b"single-use-nonce",
    )
    .expect("valid request stages");

    let terminal = newt_core::sas_confirm::recompute_sas(&candidate, ISSUER, "operator")
        .expect("the terminal must be able to recompute what the web staged");

    assert_eq!(
        response.sas_words,
        terminal.iter().map(|w| w.to_string()).collect::<Vec<_>>(),
        "browser and terminal must derive the same SAS"
    );
    assert_eq!(response.sas_words.len(), 6);
}

/// A candidate is only useful if the terminal can verify it, so the staged
/// transcript must be the one its own inputs produce.
#[test]
fn the_staged_candidate_is_self_consistent() {
    let (candidate, _) = prepare(
        Some(&rp()),
        Some(&context()),
        &request(ORIGIN, RP_ID),
        b"nonce",
    )
    .unwrap();

    assert_eq!(candidate.rp_id, RP_ID);
    assert_eq!(candidate.cose_alg, -7);
    assert!(!candidate.commitment.is_empty());
    assert!(
        newt_core::sas_confirm::recompute_sas(&candidate, ISSUER, "operator").is_some(),
        "the claimed transcript must match the candidate's own inputs"
    );
    // Bound to the issuer: another operator cannot adopt this candidate.
    assert!(
        newt_core::sas_confirm::recompute_sas(&candidate, "someone-else", "operator").is_none()
    );
}

// --- refusals ---

#[test]
fn staging_refuses_a_foreign_origin_and_a_foreign_rp() {
    let wrong_origin = prepare(
        Some(&rp()),
        Some(&context()),
        &request("https://evil.test", RP_ID),
        b"n",
    );
    assert!(matches!(wrong_origin, Err(FinishError::Rejected(_))));

    let wrong_rp = prepare(
        Some(&rp()),
        Some(&context()),
        &request(ORIGIN, "evil.example"),
        b"n",
    );
    assert!(
        matches!(wrong_rp, Err(FinishError::Rejected(_))),
        "a credential registered for another rp must not stage here"
    );
}

/// Fail-closed: with no relying party or no operator identity there is nothing
/// to bind a transcript to, so staging refuses rather than guessing.
#[test]
fn staging_refuses_when_unconfigured() {
    let req = request(ORIGIN, RP_ID);
    assert_eq!(
        prepare(None, Some(&context()), &req, b"n").unwrap_err(),
        FinishError::NotConfigured
    );
    assert_eq!(
        prepare(Some(&rp()), None, &req, b"n").unwrap_err(),
        FinishError::NotConfigured
    );
}

#[test]
fn staging_refuses_malformed_fields() {
    for (field, mutate) in [
        ("credential_id", 0usize),
        ("attestation_object", 1),
        ("client_data_json", 2),
        ("blinding", 3),
    ] {
        let mut req = request(ORIGIN, RP_ID);
        match mutate {
            0 => req.credential_id = "!!!".into(),
            1 => req.attestation_object = "!!!".into(),
            2 => req.client_data_json = "!!!".into(),
            _ => req.blinding = "!!!".into(),
        }
        assert_eq!(
            prepare(Some(&rp()), Some(&context()), &req, b"n").unwrap_err(),
            FinishError::MalformedField(field)
        );
    }
}

// --- CSP + SRI ---

/// A nonce that repeats is not a nonce: an injected script from an earlier
/// response would carry a token the current page still honours.
#[test]
fn every_response_gets_a_fresh_nonce() {
    let a = Nonce::fresh();
    let b = Nonce::fresh();
    assert_ne!(a, b);
    assert!(
        a.as_str().len() >= 16,
        "nonce must not be trivially guessable"
    );
}

/// `script-src` must offer injected script no fallback at all.
///
/// **C3b narrowed this rather than deleting it.** It used to assert that
/// `'unsafe-inline'` appears nowhere in the policy, which stopped being true
/// when `style-src-attr 'unsafe-inline'` was added — a relaxation measured to
/// be necessary (Mermaid styles its generated SVG through per-node `style=`
/// attributes: 49 blocked attribute applications against 4 blocked `<style>`
/// elements on a real page) and proven unreachable by an attacker (ammonia's
/// default allowlist carries no `style` attribute on any tag, so untrusted
/// markup cannot emit one).
///
/// Deleting the check would have been the easy move and the wrong one: it is
/// the only thing standing between this policy and `script-src 'unsafe-inline'`.
/// So it now asserts PER DIRECTIVE, and counts occurrences, which is strictly
/// stronger than the blanket test it replaces — a second relaxation anywhere
/// fails it.
#[test]
fn the_policy_denies_by_default_and_allows_no_unsafe_script() {
    let nonce = Nonce::fresh();
    let p = policy(&nonce);

    assert!(p.starts_with("default-src 'none'"), "{p}");
    assert!(
        p.contains(&format!("script-src 'nonce-{}'", nonce.as_str())),
        "{p}"
    );

    let directive = |name: &str| {
        p.split(';')
            .map(str::trim)
            .find(|d| d.split_whitespace().next() == Some(name))
            .unwrap_or("")
            .to_string()
    };

    // Script gets no fallback of any kind.
    let script = directive("script-src");
    for forbidden in [
        "'unsafe-inline'",
        "'unsafe-eval'",
        "'unsafe-hashes'",
        "'strict-dynamic'",
        "*",
        "https:",
        "data:",
    ] {
        assert!(
            !script.contains(forbidden),
            "script-src must not contain {forbidden}: {script}"
        );
    }
    // Neither does a style ELEMENT — the high-value target, and the one
    // untrusted markup would reach first if the sanitizer ever regressed.
    for name in ["style-src", "style-src-elem", "default-src"] {
        let d = directive(name);
        assert!(
            !d.contains("'unsafe-inline'"),
            "{name} must stay strict: {d}"
        );
    }
    // Exactly one relaxation exists, and it is the measured one.
    assert_eq!(
        p.matches("'unsafe-inline'").count(),
        1,
        "exactly one directive may carry 'unsafe-inline': {p}"
    );
    assert!(
        directive("style-src-attr").contains("'unsafe-inline'"),
        "…and it must be style-src-attr: {p}"
    );

    for required in [
        "base-uri 'none'",
        "frame-ancestors 'none'",
        "object-src 'none'",
        "form-action 'self'",
    ] {
        assert!(p.contains(required), "policy must contain {required}: {p}");
    }
}

/// C3b: the page tells the client what its own policy permits, derived from
/// the policy TEXT so the two cannot drift.
#[test]
fn the_style_element_capability_is_read_off_the_policy() {
    let nonce = Nonce::fresh();
    assert!(
        !newt_web::csp::permits_inline_style_elements(&policy(&nonce)),
        "this policy blocks inline style elements, and must say so"
    );
    // Anti-vacuous: it reports TRUE for a policy that does permit them, so a
    // hardcoded `false` cannot masquerade as the derivation.
    assert!(newt_web::csp::permits_inline_style_elements(
        "default-src 'none'; style-src-elem 'unsafe-inline'"
    ));
    // …and falls back to `style-src` when the element directive is absent.
    assert!(newt_web::csp::permits_inline_style_elements(
        "style-src 'unsafe-inline'"
    ));
    assert!(!newt_web::csp::permits_inline_style_elements(
        "style-src 'self'"
    ));
}

#[test]
fn sri_digests_are_sha384_and_content_bound() {
    let digest = sri(b"some asset");
    assert!(digest.starts_with("sha384-"), "{digest}");
    assert_ne!(
        digest,
        sri(b"some asset "),
        "a byte change must change the digest"
    );
    assert_eq!(digest, sri(b"some asset"), "and must be stable");
}

/// The page must carry the nonce on every script tag and an SRI digest on every
/// external one — a tag missing either is a hole in exactly the protection this
/// slice exists to add.
#[test]
fn the_page_binds_every_script_tag() {
    let nonce = Nonce::fresh();
    let page = render_page(&nonce, RP_ID, "");

    let nonce_attr = format!(r#"nonce="{}""#, nonce.as_str());
    assert_eq!(
        page.matches("<script").count(),
        page.matches(nonce_attr.as_str()).count(),
        "every script tag carries the nonce: {page}"
    );
    assert!(page.contains(&sri(HTMX_JS.as_bytes())), "htmx SRI present");
    assert!(
        page.contains(&sri(WEBAUTHN_JS.as_bytes())),
        "webauthn.js SRI present"
    );
    assert!(page.contains(r#"data-rp-id="newt.example""#));
    // The page must never claim it can finish the ceremony by itself.
    assert!(page.contains("terminal"), "the page points at the terminal");
}

/// With no relying party the page still renders, but says so and carries no rp
/// id for the script to use.
#[test]
fn an_unconfigured_page_announces_itself() {
    let page = render_page(&Nonce::fresh(), "", "<p>unavailable</p>");
    assert!(page.contains("unavailable"));
    assert!(page.contains(r#"data-rp-id="""#));
}

/// The vendored script is served as-is, so its digest is computed over what the
/// browser actually receives.
#[test]
fn the_vendored_script_is_the_one_we_hash() {
    assert!(WEBAUTHN_JS.contains("newtB64u"), "shared helpers present");
    assert!(
        WEBAUTHN_JS.contains("newtSignVerdict"),
        "interceptor present"
    );
    assert!(
        !WEBAUTHN_JS.contains("eval("),
        "no eval — the CSP would refuse it anyway, but it must not be there"
    );
}
