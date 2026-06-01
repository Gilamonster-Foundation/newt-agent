//! Read-only consumption of a peer's signed [`Caveats`].
//!
//! A peer's [`Caveats`] live inside its [`CertChain`] — they are part of the
//! signed [`AgentMetadata`] payload, so they cannot be tampered with without
//! invalidating the cert. This module surfaces the *verified* caveats at the
//! mesh boundary so worker code (35b) can ask "what authority did this peer
//! actually arrive carrying?" without re-implementing the verification dance
//! or accidentally trusting unsigned bytes.
//!
//! Scope (issue #35 phase 1a): **read-only**. Verify the cert chain end to
//! end, then hand back a borrowed reference to the caveats. No enforcement
//! decision is made here — that belongs to the worker layer in 35b.
//!
//! The verification path is the same one agent-mesh runs internally
//! ([`CertChain::verify`]): it walks the chain to a root [`Issuer::User`],
//! checks every signature, and re-checks attenuation at every link. A forged
//! or amplifying chain is rejected even if each individual signature is
//! valid.
//!
//! # Example
//!
//! ```no_run
//! # use agent_mesh_core::{AgentKey, AgentMetadata, Caveats, UserKey};
//! # use newt_mesh::caveats::{caveats_for_peer, CaveatsError};
//! # fn demo() -> Result<(), CaveatsError> {
//! # let user = UserKey::generate();
//! # let agent = AgentKey::issue(&user, AgentMetadata {
//! #     role: "peer".into(), host: "h".into(), capabilities: vec![],
//! #     issued_at: "2026-05-31T00:00:00Z".into(), expires_at: None,
//! #     caveats: Caveats::top(),
//! # });
//! let cert = agent.cert();
//! let caveats = caveats_for_peer(cert)?;
//! // ... worker code (35b) will consult `caveats` here.
//! # let _ = caveats;
//! # Ok(())
//! # }
//! ```

use agent_mesh_core::{Caveats, CertChain, MeshError};
use thiserror::Error;

/// Errors surfaced when reading a peer's signed caveats.
///
/// Wraps [`MeshError`] variants relevant to caveat extraction; we deliberately
/// split them out here so the worker layer can distinguish "the chain doesn't
/// verify" from "the chain verifies but tries to amplify authority" without
/// matching against every [`MeshError`] variant.
#[derive(Debug, Error)]
pub enum CaveatsError {
    /// The cert chain failed signature verification (a signature did not
    /// match, or a structural inconsistency in the chain was detected).
    /// Mirrors [`MeshError::BadSignature`] and [`MeshError::InvalidCertChain`].
    #[error("peer cert chain verification failed: {0}")]
    Verification(String),

    /// A link in the chain claimed strictly more authority than its parent.
    /// Mirrors [`MeshError::CaveatAmplification`]. The chain is rejected
    /// even if every individual signature is valid.
    #[error("peer cert chain amplifies authority along its delegation chain")]
    Amplification,
}

impl From<MeshError> for CaveatsError {
    fn from(err: MeshError) -> Self {
        match err {
            MeshError::CaveatAmplification => Self::Amplification,
            other => Self::Verification(other.to_string()),
        }
    }
}

/// Extract the signed [`Caveats`] from a peer's [`CertChain`].
///
/// Verifies the chain end to end (rooting at a [`UserKey`](agent_mesh_core::UserKey),
/// checking every signature, and re-checking attenuation at every link) and
/// then returns a borrowed reference to the caveats on the *leaf* metadata
/// — i.e. the authority *this peer itself* holds, which is by construction
/// `⊑` every ancestor along the chain.
///
/// # Errors
///
/// - [`CaveatsError::Verification`] if any signature in the chain fails or
///   the chain is structurally inconsistent.
/// - [`CaveatsError::Amplification`] if any link grants more authority than
///   its parent.
pub fn caveats_for_peer(cert: &CertChain) -> Result<&Caveats, CaveatsError> {
    cert.verify()?;
    Ok(&cert.metadata.caveats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_mesh_core::{AgentKey, AgentMetadata, CountBound, Scope, UserKey};

    fn fixture_metadata(role: &str, caveats: Caveats) -> AgentMetadata {
        AgentMetadata {
            role: role.to_string(),
            host: "test-host".to_string(),
            capabilities: vec!["test".to_string()],
            issued_at: "2026-05-31T00:00:00Z".to_string(),
            expires_at: None,
            caveats,
        }
    }

    /// (a) AgentMetadata with valid signed (top) Caveats → returns Ok(&caveats).
    #[test]
    fn extracts_top_caveats_from_valid_cert() {
        let user = UserKey::generate();
        let agent = AgentKey::issue(&user, fixture_metadata("peer", Caveats::top()));
        let cert = agent.cert();

        let caveats = caveats_for_peer(cert).expect("valid cert should extract caveats");
        assert_eq!(caveats, &Caveats::top());
    }

    /// (b) AgentMetadata with a structurally invalid (mismatched) cert →
    /// returns Verification error.
    ///
    /// agent-mesh's `AgentMetadata` cannot exist *without* caveats (they
    /// have a serde-default of `⊤`), so the "no caveats present" case is
    /// modeled here as "the cert this metadata lives in is invalid" — the
    /// equivalent failure surface at the worker boundary, where we never
    /// see bare AgentMetadata, only metadata-via-verified-CertChain.
    #[test]
    fn rejects_tampered_metadata_with_verification_error() {
        let user = UserKey::generate();
        let agent = AgentKey::issue(&user, fixture_metadata("peer", Caveats::top()));
        let mut cert = agent.cert().clone();
        // Mutate signed metadata after issue → signature no longer matches.
        cert.metadata.role = "evil".to_string();

        match caveats_for_peer(&cert) {
            Err(CaveatsError::Verification(msg)) => {
                assert!(
                    msg.contains("signature") || msg.contains("verification"),
                    "expected signature failure, got: {msg}"
                );
            }
            other => panic!("expected Verification error, got: {other:?}"),
        }
    }

    /// (c) AgentMetadata with INVALID signature → returns Verification error.
    ///
    /// Mutating `agent_pubkey` after issue makes the verifier recompute the
    /// canonical signing payload with the wrong public key — the signature
    /// then fails to verify, producing [`MeshError::BadSignature`] which our
    /// `From` impl folds into `CaveatsError::Verification`.
    #[test]
    fn rejects_invalid_signature_with_verification_error() {
        let user = UserKey::generate();
        let agent = AgentKey::issue(&user, fixture_metadata("peer", Caveats::top()));
        let mut cert = agent.cert().clone();
        cert.agent_pubkey[0] ^= 0xff;

        assert!(matches!(
            caveats_for_peer(&cert),
            Err(CaveatsError::Verification(_))
        ));
    }

    /// (d) Round-trip: construct AgentMetadata carrying a non-trivial
    /// (attenuated) Caveats, extract via caveats_for_peer, and assert the
    /// extracted reference equals the input structurally.
    #[test]
    fn roundtrips_attenuated_caveats() {
        let attenuated = Caveats {
            exec: Scope::only(["git".to_string(), "cargo".to_string()]),
            net: Scope::none(),
            max_calls: CountBound::AtMost(8),
            valid_for_generation: Scope::only([42u64]),
            ..Caveats::top()
        };
        let user = UserKey::generate();
        let agent = AgentKey::issue(
            &user,
            fixture_metadata("attenuated-peer", attenuated.clone()),
        );

        let caveats = caveats_for_peer(agent.cert()).expect("valid attenuated cert verifies");
        assert_eq!(caveats, &attenuated);
        // Sanity: attenuated authority is strictly below ⊤.
        assert!(caveats.leq(&Caveats::top()));
        assert!(!Caveats::top().leq(caveats));
    }

    /// Delegated (multi-link) chain whose every link is properly attenuated
    /// extracts the *leaf* caveats — the authority the immediate peer holds,
    /// which is by construction `⊑` every ancestor.
    #[test]
    fn extracts_leaf_caveats_from_delegated_chain() {
        let user = UserKey::generate();
        let parent = AgentKey::issue(
            &user,
            fixture_metadata(
                "parent",
                Caveats {
                    exec: Scope::only(["git".to_string(), "cargo".to_string()]),
                    ..Caveats::top()
                },
            ),
        );
        let child_caveats = Caveats {
            exec: Scope::only(["git".to_string()]),
            ..Caveats::top()
        };
        let child = parent
            .delegate(fixture_metadata("child", child_caveats.clone()))
            .expect("attenuating delegation");

        let leaf = caveats_for_peer(child.cert()).expect("delegated chain verifies");
        assert_eq!(leaf, &child_caveats);
        // The leaf is strictly attenuated from the parent.
        assert!(leaf.leq(&parent.cert().metadata.caveats));
    }

    /// `From<MeshError>` collapses non-amplification variants into
    /// `Verification` while preserving the dedicated `Amplification` slot.
    #[test]
    fn mesh_error_conversion_maps_variants() {
        let amp: CaveatsError = MeshError::CaveatAmplification.into();
        assert!(matches!(amp, CaveatsError::Amplification));

        let bad_sig: CaveatsError = MeshError::BadSignature.into();
        assert!(matches!(bad_sig, CaveatsError::Verification(_)));

        let bad_chain: CaveatsError = MeshError::InvalidCertChain("oops".into()).into();
        assert!(matches!(bad_chain, CaveatsError::Verification(_)));
    }
}
