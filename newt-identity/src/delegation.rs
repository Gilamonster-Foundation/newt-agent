//! Receive the existing signed plugin envelope under a trusted launch anchor.
//!
//! This verifies inherited authority, not the identity of a connecting process.
//! A certificate and its content ID are public: echoing them is not proof of
//! possession of the child's signing key. The launcher must separately bind
//! delivery to its intended child over an owned or authenticated channel.

use agent_mesh_protocol::CertChain;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use content_addressable::RawContentId;
use newt_core::Caveats;

/// Verified inherited authority. This runtime wrapper is not a new wire format
/// and contains no private key. Only the checked receiver may construct it.
pub struct VerifiedDelegation {
    cert: CertChain,
}

impl VerifiedDelegation {
    /// The immutable parent ceiling; callers may meet with it, never mutate it.
    pub fn caveats(&self) -> &Caveats {
        &self.cert.metadata.caveats
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DelegationError {
    #[error("invalid delegated authority envelope")]
    InvalidEnvelope,
    #[error("invalid delegated authority certificate")]
    InvalidCertificate,
    #[error("delegated authority does not match the trusted launch anchor")]
    UnexpectedEnvelope,
}

/// Verify an existing base64/JSON `CertChain` against the exact envelope named
/// by the trusted launcher. `expected` must NOT be learned from the received
/// envelope: it pins the parent's freshly delegated child and its entire chain.
/// Transport lifetime, replay prevention, and receiver authentication remain
/// obligations of the launcher; this function does not establish them.
pub fn verify_delegation(
    envelope: &[u8],
    _expected: &RawContentId,
) -> Result<VerifiedDelegation, DelegationError> {
    // Tests-first seam: retain the existing cert verification behavior until
    // the exact-launch-anchor regression has run.
    let bytes = B64
        .decode(envelope)
        .map_err(|_| DelegationError::InvalidEnvelope)?;
    let cert: CertChain =
        serde_json::from_slice(&bytes).map_err(|_| DelegationError::InvalidEnvelope)?;
    cert.verify()
        .map_err(|_| DelegationError::InvalidCertificate)?;
    Ok(VerifiedDelegation { cert })
}

#[cfg(test)]
#[path = "delegation_tests.rs"]
mod tests;
