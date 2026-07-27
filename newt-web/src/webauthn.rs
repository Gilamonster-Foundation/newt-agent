//! The relying-party boundary: the only way newt-web verifies a WebAuthn proof.
//!
//! agent-bridle's verifiers check the *signature*. They do not check that the
//! assertion was produced for **this** relying party — that is the caller's job,
//! and a caller who forgets it accepts a valid signature made for any other
//! site the credential is registered with. So the raw verifiers are private to
//! this module and unreachable from the rest of the crate; the only exported
//! entry point is [`RelyingParty::verify_assertion`], which refuses before it
//! ever reaches a signature check unless:
//!
//! * `clientDataJSON.origin` equals the configured origin **exactly** —
//!   scheme, host, and port. Not a suffix match, which `evil-newt.example`
//!   would pass against `newt.example`.
//! * `authenticatorData[0..32]` equals `SHA-256(rp_id)`.
//!
//! Configuration is injected and **fail-closed**: with `NEWT_WEB_RP_ID` or
//! `NEWT_WEB_ORIGIN` unset there is no [`RelyingParty`] to call, so the
//! deployment cannot accidentally run with origin checking disabled. There is
//! deliberately no default and no "permissive" mode.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use sha2::{Digest, Sha256};

/// COSE algorithm identifier for ECDSA-P256-SHA256.
const COSE_ES256: i64 = -7;
/// COSE algorithm identifier for EdDSA (Ed25519).
const COSE_EDDSA: i64 = -8;

/// Why a relying-party check refused. Every variant is a refusal; there is no
/// success variant, because success is `Ok(())`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpError {
    /// `NEWT_WEB_RP_ID` or `NEWT_WEB_ORIGIN` was unset or empty.
    NotConfigured(&'static str),
    /// The assertion carried no `clientDataJSON` / `authenticatorData`, so it
    /// is not a WebAuthn proof at all.
    NotWebAuthn,
    /// `clientDataJSON` was not valid UTF-8 JSON with a string `origin`.
    MalformedClientData,
    /// The origin did not match exactly.
    OriginMismatch { expected: String, got: String },
    /// `authenticatorData` was shorter than the 32-byte rpIdHash it must start
    /// with, or the hash did not match.
    RpIdHashMismatch,
    /// The credential's COSE algorithm is not one we accept.
    UnsupportedAlgorithm(i64),
    /// The attestation object could not be parsed, or carried no COSE key.
    MalformedAttestation,
    /// The signature itself failed to verify.
    BadSignature(String),
}

impl std::fmt::Display for RpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured(var) => write!(f, "{var} is unset; refusing every assertion"),
            Self::NotWebAuthn => write!(f, "not a WebAuthn assertion"),
            Self::MalformedClientData => write!(f, "malformed clientDataJSON"),
            Self::OriginMismatch { expected, got } => {
                write!(f, "origin {got:?} is not {expected:?}")
            }
            Self::RpIdHashMismatch => write!(f, "rpIdHash does not match the configured rp_id"),
            Self::UnsupportedAlgorithm(alg) => write!(f, "COSE algorithm {alg} is not accepted"),
            Self::MalformedAttestation => write!(f, "malformed attestation object"),
            Self::BadSignature(why) => write!(f, "signature rejected: {why}"),
        }
    }
}

impl std::error::Error for RpError {}

/// The algorithms this deployment accepts, and nothing else.
///
/// Dispatch is exhaustive with an explicit deny arm: adding a COSE identifier
/// to the wire does not silently widen what we verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoseAlg {
    /// ES256 (`-7`) — the primary, and the only algorithm every authenticator
    /// is required to implement.
    Es256,
    /// EdDSA / Ed25519 (`-8`).
    Ed25519,
}

impl CoseAlg {
    /// Map a COSE identifier, refusing anything unlisted.
    pub fn from_cose(alg: i64) -> Result<Self, RpError> {
        match alg {
            COSE_ES256 => Ok(Self::Es256),
            COSE_EDDSA => Ok(Self::Ed25519),
            other => Err(RpError::UnsupportedAlgorithm(other)),
        }
    }

    /// The COSE identifier this maps back to.
    #[must_use]
    pub fn as_cose(self) -> i64 {
        match self {
            Self::Es256 => COSE_ES256,
            Self::Ed25519 => COSE_EDDSA,
        }
    }
}

/// A credential's public key as the authenticator attested it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttestedKey {
    /// The credential id (WebAuthn `rawId`).
    pub credential_id: Vec<u8>,
    /// Canonical COSE public-key bytes, as they appear in the attestation.
    pub cose_pubkey: Vec<u8>,
    /// The algorithm the key declares.
    pub alg: CoseAlg,
}

/// The configured relying party. Holding one is proof the deployment supplied
/// both an rp id and an origin.
#[derive(Debug, Clone)]
pub struct RelyingParty {
    rp_id: String,
    origin: String,
    rp_id_hash: [u8; 32],
}

impl RelyingParty {
    /// Build from explicit values. Used by [`Self::from_env`] and by tests;
    /// both fields are required, so there is no way to construct a partially
    /// configured relying party.
    pub fn new(rp_id: impl Into<String>, origin: impl Into<String>) -> Result<Self, RpError> {
        let rp_id = rp_id.into();
        let origin = origin.into();
        if rp_id.trim().is_empty() {
            return Err(RpError::NotConfigured("NEWT_WEB_RP_ID"));
        }
        if origin.trim().is_empty() {
            return Err(RpError::NotConfigured("NEWT_WEB_ORIGIN"));
        }
        let rp_id_hash = Sha256::digest(rp_id.as_bytes()).into();
        Ok(Self {
            rp_id,
            origin,
            rp_id_hash,
        })
    }

    /// Read `NEWT_WEB_RP_ID` and `NEWT_WEB_ORIGIN`. Fail-closed: an unset or
    /// blank variable yields `Err`, never a permissive default.
    pub fn from_env() -> Result<Self, RpError> {
        let rp_id = std::env::var("NEWT_WEB_RP_ID")
            .map_err(|_| RpError::NotConfigured("NEWT_WEB_RP_ID"))?;
        let origin = std::env::var("NEWT_WEB_ORIGIN")
            .map_err(|_| RpError::NotConfigured("NEWT_WEB_ORIGIN"))?;
        Self::new(rp_id, origin)
    }

    /// The configured relying-party id.
    #[must_use]
    pub fn rp_id(&self) -> &str {
        &self.rp_id
    }

    /// The configured origin.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Check that `client_data_json` was produced for this origin.
    ///
    /// Exact string equality on the whole origin. A prefix or suffix test would
    /// accept `https://newt.example.evil.test` for `https://newt.example`.
    pub fn check_origin(&self, client_data_json: &[u8]) -> Result<(), RpError> {
        let value: serde_json::Value =
            serde_json::from_slice(client_data_json).map_err(|_| RpError::MalformedClientData)?;
        let got = value
            .get("origin")
            .and_then(serde_json::Value::as_str)
            .ok_or(RpError::MalformedClientData)?;
        if got == self.origin {
            Ok(())
        } else {
            Err(RpError::OriginMismatch {
                expected: self.origin.clone(),
                got: got.to_owned(),
            })
        }
    }

    /// Check that `authenticator_data` begins with `SHA-256(rp_id)`.
    pub fn check_rp_id_hash(&self, authenticator_data: &[u8]) -> Result<(), RpError> {
        let front = authenticator_data
            .get(..32)
            .ok_or(RpError::RpIdHashMismatch)?;
        // Constant-time-ish: compare the whole prefix, never short-circuiting on
        // the first differing byte.
        let differing = front
            .iter()
            .zip(self.rp_id_hash.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b));
        if differing == 0 {
            Ok(())
        } else {
            Err(RpError::RpIdHashMismatch)
        }
    }

    /// Both relying-party checks, in the order a reviewer expects to find them.
    ///
    /// Every caller that verifies a signature must pass through here first —
    /// that is the whole point of keeping the underlying verifiers private.
    pub fn check_bindings(
        &self,
        client_data_json: Option<&[u8]>,
        authenticator_data: Option<&[u8]>,
    ) -> Result<(), RpError> {
        let (client_data, auth_data) = client_data_json
            .zip(authenticator_data)
            .ok_or(RpError::NotWebAuthn)?;
        self.check_origin(client_data)?;
        self.check_rp_id_hash(auth_data)
    }
}

/// Extract the credential id, COSE public key, and algorithm from a WebAuthn
/// `attestationObject`.
///
/// Only the fields we need are read. `fmt` is ignored on purpose: this is the
/// `attestation: "none"` path, an accepted residual risk of #1366 — SAS plus
/// user-verification is what catches a cloned authenticator, not model
/// attestation.
pub fn parse_attestation(attestation_object: &[u8]) -> Result<AttestedKey, RpError> {
    let value: ciborium::value::Value =
        ciborium::from_reader(attestation_object).map_err(|_| RpError::MalformedAttestation)?;
    let auth_data = cbor_map_get(&value, "authData")
        .and_then(|v| v.as_bytes().cloned())
        .ok_or(RpError::MalformedAttestation)?;
    parse_authenticator_data(&auth_data)
}

/// Pull the attested credential out of raw `authenticatorData`.
///
/// Layout per the WebAuthn spec: `rpIdHash(32) ‖ flags(1) ‖ signCount(4)`, then
/// — when the attested-credential-data flag (bit 6) is set —
/// `aaguid(16) ‖ credIdLen(2, big-endian) ‖ credId ‖ COSEKey`.
pub fn parse_authenticator_data(auth_data: &[u8]) -> Result<AttestedKey, RpError> {
    const HEADER: usize = 32 + 1 + 4;
    const AAGUID: usize = 16;

    let flags = *auth_data.get(32).ok_or(RpError::MalformedAttestation)?;
    if flags & 0b0100_0000 == 0 {
        return Err(RpError::MalformedAttestation);
    }
    let len_at = HEADER + AAGUID;
    let cred_len = auth_data
        .get(len_at..len_at + 2)
        .ok_or(RpError::MalformedAttestation)?;
    let cred_len = u16::from_be_bytes([cred_len[0], cred_len[1]]) as usize;
    let key_at = len_at + 2 + cred_len;
    let credential_id = auth_data
        .get(len_at + 2..key_at)
        .ok_or(RpError::MalformedAttestation)?
        .to_vec();
    let cose_pubkey = auth_data
        .get(key_at..)
        .filter(|rest| !rest.is_empty())
        .ok_or(RpError::MalformedAttestation)?
        .to_vec();

    let key: ciborium::value::Value =
        ciborium::from_reader(cose_pubkey.as_slice()).map_err(|_| RpError::MalformedAttestation)?;
    // COSE label 3 is `alg`.
    let alg = cbor_map_get_int(&key, 3)
        .and_then(|v| v.as_integer())
        .and_then(|i| i64::try_from(i).ok())
        .ok_or(RpError::MalformedAttestation)?;

    Ok(AttestedKey {
        credential_id,
        cose_pubkey,
        alg: CoseAlg::from_cose(alg)?,
    })
}

/// The base64url-encoded challenge `clientDataJSON` claims, for binding against
/// the challenge the server issued.
pub fn client_data_challenge(client_data_json: &[u8]) -> Result<Vec<u8>, RpError> {
    let value: serde_json::Value =
        serde_json::from_slice(client_data_json).map_err(|_| RpError::MalformedClientData)?;
    let encoded = value
        .get("challenge")
        .and_then(serde_json::Value::as_str)
        .ok_or(RpError::MalformedClientData)?;
    B64URL
        .decode(encoded)
        .map_err(|_| RpError::MalformedClientData)
}

fn cbor_map_get<'a>(
    value: &'a ciborium::value::Value,
    key: &str,
) -> Option<&'a ciborium::value::Value> {
    value
        .as_map()?
        .iter()
        .find(|(k, _)| k.as_text() == Some(key))
        .map(|(_, v)| v)
}

fn cbor_map_get_int(value: &ciborium::value::Value, key: i64) -> Option<&ciborium::value::Value> {
    value
        .as_map()?
        .iter()
        .find(|(k, _)| {
            k.as_integer()
                .and_then(|i| i64::try_from(i).ok())
                .is_some_and(|i| i == key)
        })
        .map(|(_, v)| v)
}
