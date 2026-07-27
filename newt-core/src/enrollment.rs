//! Passkey enrollment: staged in the store by the web, promoted to durable
//! authority only by the terminal.
//!
//! The enrollment path is the one place a web actor could otherwise mint
//! standing authority for itself, so the split is structural rather than
//! procedural. The web can reach [`ConversationStore::publish_enrollment_candidate`]
//! and nothing else; that writes a proposal that expires. Only
//! [`answer_enrollment_request_as`] — which requires the operator root key the
//! web never holds — turns a proposal into a signed registry row. A web actor
//! that fully controls the staging call still cannot enroll a credential.
//!
//! This mirrors the standing rule that a web grant is ephemeral and only a
//! terminal audit promotes it to durable OCAP.

use std::path::{Path, PathBuf};

use agent_mesh_protocol::UserKey;
use serde::{Deserialize, Serialize};

use crate::credential_registry::{append_credential, CredentialRecord};
use crate::store::ConversationStore;

/// What the browser proposes: the credential the authenticator just produced,
/// plus the transcript both ends derived independently.
///
/// Every field here is *claimed* by the staging surface. None of it is trusted
/// until the terminal confirms the transcript's word string matches what the
/// human sees in the browser — that comparison is what authenticates the claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollmentCandidate {
    /// The authenticator's credential id (WebAuthn `rawId`), base64.
    pub credential_id_handle: String,
    /// Canonical COSE public-key bytes, base64.
    pub cose_pubkey: String,
    /// COSE algorithm identifier (`-7` ES256, `-8` Ed25519).
    pub cose_alg: i64,
    /// Fingerprint of the mesh agent that ran the ceremony.
    pub mesh_agent_fingerprint: String,
    /// Hex transcript id the staging surface *claims*.
    ///
    /// Never displayed and never trusted: the terminal rebuilds the transcript
    /// from the inputs below and refuses the candidate if the two disagree. It
    /// is carried only so that disagreement is detectable.
    pub transcript_id: String,
    /// Relying-party id the ceremony ran under.
    pub rp_id: String,
    /// Hex commitment the browser published before the nonce was revealed.
    pub commitment: String,
    /// The server's single-use enrollment nonce, base64.
    pub enroll_nonce: String,
}

impl EnrollmentCandidate {
    /// The registry row this candidate becomes once an operator signs it.
    #[must_use]
    pub fn into_record(self, issued_generation: u64) -> CredentialRecord {
        CredentialRecord {
            credential_id_handle: self.credential_id_handle,
            cose_pubkey: self.cose_pubkey,
            cose_alg: self.cose_alg,
            mesh_agent_fingerprint: self.mesh_agent_fingerprint,
            issued_generation,
            transcript_id: self.transcript_id,
            revoked: false,
            sig: None,
        }
    }
}

/// Promote a staged candidate to a signed registry binding — the only path from
/// web staging to durable authority, and it runs only on a terminal `y`.
///
/// Consumes the candidate exactly once before writing, so a replayed
/// confirmation cannot enroll twice. Returns the file written, or `Ok(None)`
/// when there is nothing promotable under `request_id` — unknown, already
/// taken, declined, or expired all look the same to the caller on purpose.
pub fn answer_enrollment_request_as(
    store: &ConversationStore,
    conversation_id: &str,
    request_id: &str,
    config_path: &Path,
    subject: &str,
    issued_generation: u64,
    root_key: &UserKey,
) -> anyhow::Result<Option<PathBuf>> {
    let Some(candidate_json) = store.take_enrollment_candidate(conversation_id, request_id)? else {
        return Ok(None);
    };
    let candidate: EnrollmentCandidate = serde_json::from_str(&candidate_json)?;
    let record = candidate.into_record(issued_generation);
    append_credential(config_path, subject, record, root_key).map(Some)
}
