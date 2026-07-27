//! `GET /enroll` and `POST /enroll/finish` — the browser half of the ceremony.
//!
//! `/enroll/finish` stages a candidate and nothing more. It cannot enroll:
//! promotion needs the operator root key, which lives at the terminal, and the
//! staging table has no web-writable verdict column at all (#1369). So the
//! worst a fully compromised browser achieves here is a proposal that expires.
//!
//! Both relying-party checks run before anything is staged, and the transcript
//! is derived server-side from the values the browser committed to — the
//! browser is never trusted to supply a transcript.

use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use base64::engine::general_purpose::STANDARD as B64;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::csp::{hardening_headers, policy, sri, Nonce};
use crate::webauthn::{parse_attestation, RelyingParty};

/// The operator identity the transcript is bound to.
///
/// `issuer` MUST be the operator root fingerprint the terminal will recompute
/// with — the transcript binds it, so a mismatch makes the two ends derive
/// different words and the ceremony correctly fails to confirm. Injected and
/// fail-closed for the same reason the relying party is: there is no sensible
/// default, and guessing one would produce a ceremony that silently never
/// matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollmentContext {
    pub issuer: String,
    pub subject: String,
}

impl EnrollmentContext {
    /// Read `NEWT_WEB_ISSUER` (operator root fingerprint, hex) and
    /// `NEWT_WEB_SUBJECT` (defaulting to `operator`, which is the registry's
    /// own default bundle name).
    pub fn from_env() -> Result<Self, FinishError> {
        let issuer = std::env::var("NEWT_WEB_ISSUER")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or(FinishError::NotConfigured)?;
        let subject = std::env::var("NEWT_WEB_SUBJECT")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "operator".to_string());
        Ok(Self { issuer, subject })
    }
}

/// What the browser posts at the end of a `navigator.credentials.create`.
#[derive(Debug, Deserialize)]
pub struct FinishRequest {
    /// WebAuthn `rawId`, base64url.
    pub credential_id: String,
    /// `attestationObject`, base64url.
    pub attestation_object: String,
    /// `clientDataJSON`, base64url.
    pub client_data_json: String,
    /// The browser's commitment blinding, base64url.
    pub blinding: String,
}

/// What the server hands back: the words the human compares. Deliberately not
/// the transcript itself — the terminal recomputes that from its own copy of
/// the inputs, and shipping it here would invite a client that displays
/// whatever it likes.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct FinishResponse {
    /// The six-word short authentication string.
    pub sas_words: Vec<String>,
}

/// Why a staging attempt was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum FinishError {
    /// The relying party is not configured, so nothing can be verified.
    NotConfigured,
    /// A field was not valid base64url.
    MalformedField(&'static str),
    /// The relying-party bindings failed.
    Rejected(String),
    /// The candidate could not be staged.
    StagingFailed,
}

impl IntoResponse for FinishError {
    fn into_response(self) -> Response {
        let (code, message) = match self {
            // 503, not 400: the deployment is misconfigured, not the request.
            Self::NotConfigured => (
                StatusCode::SERVICE_UNAVAILABLE,
                "passkey enrollment is not configured".to_string(),
            ),
            Self::MalformedField(field) => (StatusCode::BAD_REQUEST, format!("malformed {field}")),
            Self::Rejected(why) => (StatusCode::FORBIDDEN, why),
            Self::StagingFailed => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not stage the candidate".to_string(),
            ),
        };
        (code, message).into_response()
    }
}

/// The inputs a staged candidate is built from, once the request has been
/// validated. Separated from the route so the whole decision is testable
/// without an HTTP server or a store.
#[derive(Debug, PartialEq, Eq)]
pub struct StagedInputs {
    pub credential_id_handle: String,
    pub cose_pubkey: String,
    pub cose_alg: i64,
    pub enroll_nonce: String,
    pub commitment: String,
}

/// Validate a finish request against the relying party and derive everything a
/// candidate needs.
///
/// `enroll_nonce` is supplied by the caller rather than generated here so the
/// function stays pure and the nonce's single-use bookkeeping stays with the
/// channel that owns it.
pub fn stage_inputs(
    rp: &RelyingParty,
    request: &FinishRequest,
    enroll_nonce: &[u8],
) -> Result<StagedInputs, FinishError> {
    let client_data = B64URL
        .decode(&request.client_data_json)
        .map_err(|_| FinishError::MalformedField("client_data_json"))?;
    let attestation = B64URL
        .decode(&request.attestation_object)
        .map_err(|_| FinishError::MalformedField("attestation_object"))?;
    let credential_id = B64URL
        .decode(&request.credential_id)
        .map_err(|_| FinishError::MalformedField("credential_id"))?;
    let blinding = B64URL
        .decode(&request.blinding)
        .map_err(|_| FinishError::MalformedField("blinding"))?;

    // Origin first: refuse before spending effort parsing an attestation that
    // was not even made for us.
    rp.check_origin(&client_data)
        .map_err(|e| FinishError::Rejected(e.to_string()))?;
    let attested =
        parse_attestation(&attestation).map_err(|e| FinishError::Rejected(e.to_string()))?;
    rp.check_rp_id_hash_of_attestation(&attestation)
        .map_err(|e| FinishError::Rejected(e.to_string()))?;

    let commitment = newt_core::sas_transcript::commit(&attested.cose_pubkey, &blinding);
    Ok(StagedInputs {
        credential_id_handle: B64.encode(&credential_id),
        cose_pubkey: B64.encode(&attested.cose_pubkey),
        cose_alg: attested.alg.as_cose(),
        enroll_nonce: B64.encode(enroll_nonce),
        commitment: commitment.hex(),
    })
}

/// `GET /enroll` — the ceremony page.
///
/// Carries its own nonce'd CSP and SRI digests; the nonce is minted per
/// response, so two loads never share one.
pub async fn page() -> Response {
    let nonce = Nonce::fresh();
    let rp = RelyingParty::from_env();
    let (rp_id, banner) = match &rp {
        Ok(rp) => (rp.rp_id().to_string(), String::new()),
        Err(why) => (
            String::new(),
            format!(r#"<p class="empty">enrollment unavailable — {why}</p>"#),
        ),
    };

    let body = render_page(&nonce, &rp_id, &banner);
    let mut response = Html(body).into_response();
    let headers = response.headers_mut();
    if let Ok(value) = policy(&nonce).parse() {
        headers.insert("content-security-policy", value);
    }
    for (name, value) in hardening_headers() {
        if let Ok(v) = value.parse() {
            headers.insert(name, v);
        }
    }
    response
}

/// The page body, split out so a test can read it without an HTTP round trip.
#[must_use]
pub fn render_page(nonce: &Nonce, rp_id: &str, banner: &str) -> String {
    let n = nonce.as_str();
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>newt-web — enroll a passkey</title>
<script nonce="{n}" src="/assets/htmx.min.js" integrity="{htmx_sri}" crossorigin="anonymous"></script>
<script nonce="{n}" src="/assets/webauthn.js" integrity="{wa_sri}" crossorigin="anonymous" defer></script>
</head>
<body>
<header><h1>enroll a passkey</h1></header>
<main id="enroll" data-rp-id="{rp_id}" data-subject="operator">
{banner}
<button id="enroll-start" type="button">enroll this device</button>
<p id="enroll-status"></p>
<p><strong id="enroll-sas"></strong></p>
<p>Enrollment completes at your <em>terminal</em>, never here.</p>
</main>
</body>
</html>"##,
        n = n,
        rp_id = rp_id,
        banner = banner,
        htmx_sri = sri(crate::csp::HTMX_JS.as_bytes()),
        wa_sri = sri(crate::csp::WEBAUTHN_JS.as_bytes()),
    )
}

/// Validate, then hand back both what to stage and what to display.
///
/// Staging itself is the binary's job — it owns the store handle — so this
/// stays synchronous and testable. The words are derived from the SERVER's
/// transcript, so the page can only ever display what the server already
/// committed to showing the terminal.
pub fn prepare(
    rp: Option<&RelyingParty>,
    context: Option<&EnrollmentContext>,
    request: &FinishRequest,
    enroll_nonce: &[u8],
) -> Result<(newt_core::enrollment::EnrollmentCandidate, FinishResponse), FinishError> {
    let rp = rp.ok_or(FinishError::NotConfigured)?;
    let context = context.ok_or(FinishError::NotConfigured)?;
    let staged = stage_inputs(rp, request, enroll_nonce)?;
    let words = sas_for(rp, context, &staged).ok_or(FinishError::StagingFailed)?;

    let transcript_id = transcript_of(
        rp,
        context,
        &staged.commitment,
        &staged.enroll_nonce,
        staged.cose_alg,
        &staged.cose_pubkey,
    );
    let candidate = newt_core::enrollment::EnrollmentCandidate {
        credential_id_handle: staged.credential_id_handle,
        cose_pubkey: staged.cose_pubkey,
        cose_alg: staged.cose_alg,
        mesh_agent_fingerprint: String::new(),
        transcript_id,
        rp_id: rp.rp_id().to_string(),
        commitment: staged.commitment,
        enroll_nonce: staged.enroll_nonce,
    };
    Ok((
        candidate,
        FinishResponse {
            sas_words: words.into_iter().map(str::to_string).collect(),
        },
    ))
}

/// The transcript hex a candidate claims — derived here, never accepted from
/// the browser, and recomputed independently by the terminal.
fn transcript_of(
    rp: &RelyingParty,
    context: &EnrollmentContext,
    commitment: &str,
    enroll_nonce: &str,
    cose_alg: i64,
    cose_pubkey: &str,
) -> String {
    let pubkey = B64.decode(cose_pubkey).unwrap_or_default();
    let nonce = B64.decode(enroll_nonce).unwrap_or_default();
    let Ok(commitment) = commitment.parse() else {
        return String::new();
    };
    newt_core::sas_transcript::TranscriptInputs {
        rp_id: rp.rp_id(),
        issuer: &context.issuer,
        subject: &context.subject,
        mesh_agent_fingerprint: "",
        cose_alg,
        cose_pubkey: &pubkey,
        commitment: &commitment,
        enroll_nonce: &nonce,
    }
    .transcript_id()
    .hex()
}

/// Derive the words for a staged candidate.
///
/// Returns `None` rather than substituting a placeholder if any field fails to
/// decode: showing words derived from partly-defaulted inputs would invite a
/// human to confirm a comparison that means nothing.
#[must_use]
pub fn sas_for(
    rp: &RelyingParty,
    context: &EnrollmentContext,
    staged: &StagedInputs,
) -> Option<[&'static str; 6]> {
    let cose_pubkey = B64.decode(&staged.cose_pubkey).ok()?;
    let enroll_nonce = B64.decode(&staged.enroll_nonce).ok()?;
    let commitment = staged.commitment.parse().ok()?;
    let transcript = newt_core::sas_transcript::TranscriptInputs {
        rp_id: rp.rp_id(),
        issuer: &context.issuer,
        subject: &context.subject,
        // Set by the staging channel when the mesh agent is known; the terminal
        // binds the same value, so an empty one here is still symmetric.
        mesh_agent_fingerprint: "",
        cose_alg: staged.cose_alg,
        cose_pubkey: &cose_pubkey,
        commitment: &commitment,
        enroll_nonce: &enroll_nonce,
    }
    .transcript_id();
    Some(newt_core::sas_transcript::sas_words(&transcript))
}
