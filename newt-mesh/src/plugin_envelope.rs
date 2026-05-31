//! Plugin-envelope transport — phase 1c of issue #35.
//!
//! When a Newt host spawns a provider-plugin subprocess (`newt-provider-…`),
//! it delegates an **attenuated** [`AgentKey`] to that plugin so the plugin
//! enforces caveats locally on every tool dispatch it makes. The handoff is
//! one-way (host → plugin), uses the
//! [`AGENT_KEY_ENV`](plugins_protocol::AGENT_KEY_ENV) environment variable
//! for transport, and carries a base64-encoded JSON [`CertChain`] so the
//! plugin can verify the chain end to end against the user public key
//! embedded in the cert.
//!
//! # The two halves
//!
//! ## Caller side (host) — [`serialize_for_plugin`]
//!
//! The host holds a parent [`AgentKey`] (its own per-process identity) and
//! a [`Caveats`](agent_mesh_core::Caveats) value describing the authority
//! the plugin should run with. It calls
//! [`AgentKey::delegate`](agent_mesh_core::AgentKey::delegate) to mint a
//! child key — `delegate()` *rejects* a child whose caveats amplify
//! authority (returns [`MeshError::CaveatAmplification`]) — then JSON-
//! encodes the child cert chain and base64-wraps it for env-var transport.
//!
//! The host never persists the child signing key: the plugin only needs
//! the *cert chain* to assert authority; signing operations the plugin
//! needs to perform itself (none today) would require a different
//! transport (e.g. stdin handshake with the seed bytes).
//!
//! ## Plugin side — [`caveats_from_envelope`]
//!
//! The plugin reads [`AGENT_KEY_ENV`](plugins_protocol::AGENT_KEY_ENV),
//! base64-decodes it, JSON-decodes the [`CertChain`], runs
//! [`CertChain::verify`](agent_mesh_core::CertChain::verify) — which walks
//! the chain to its root [`Issuer::User`](agent_mesh_core::Issuer::User),
//! checks every signature, *and* re-checks attenuation at every link — and
//! then converts the verified [`Caveats`](agent_mesh_core::Caveats) into
//! the local enforcement-side [`newt_core::Caveats`] mirror.
//!
//! No external trust anchor is needed: the chain self-roots at a
//! [`UserPublic`](agent_mesh_core::UserPublic) and signatures verify
//! against the embedded key. Phase 1d may pin a trust anchor for stricter
//! threat models; phase 1c accepts any validly-signed chain.
//!
//! # Threat model (phase 1c)
//!
//! - Adversary is a *confused* (not actively malicious) provider plugin —
//!   we want to prevent it from reaching beyond the authority the host
//!   delegated, not to defend against a same-uid attacker reading
//!   `/proc/$PID/environ`. Phase 1d will harden by moving the envelope
//!   off the env var onto a stdin handshake.
//!
//! - The chain is signed; tampering invalidates it. A plugin that
//!   *forges* a chain rooting at a different user public key is rejected
//!   by `verify()` (the signature won't match).
//!
//! - Attenuation is structurally enforced at both mint and verify time —
//!   a chain whose intermediate links amplify authority is rejected even
//!   if each individual signature is valid.

use agent_mesh_core::{AgentKey, AgentMetadata, CertChain, MeshError};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use thiserror::Error;

/// Errors surfaced by the plugin-envelope transport.
#[derive(Debug, Error)]
pub enum EnvelopeError {
    /// The envelope was not valid base64. Either a transport bug
    /// (something mutated the env var) or an old plugin that wrote
    /// raw JSON instead of base64'd JSON.
    #[error("envelope is not valid base64: {0}")]
    BadBase64(String),

    /// The envelope decoded but the bytes were not a valid
    /// [`CertChain`] JSON payload.
    #[error("envelope is not a valid CertChain: {0}")]
    BadCertChain(String),

    /// The envelope decoded and parsed but the cert chain failed
    /// signature verification (a signature didn't match, the chain is
    /// structurally inconsistent, or some link claimed strictly more
    /// authority than its parent).
    #[error("cert chain failed verification: {0}")]
    Verification(String),

    /// The caller asked to delegate a child whose authority is not
    /// `⊑` the parent — [`AgentKey::delegate`] refused to mint it.
    /// Wraps [`MeshError::CaveatAmplification`].
    #[error("requested child caveats amplify parent authority")]
    Amplification,
}

impl From<MeshError> for EnvelopeError {
    fn from(err: MeshError) -> Self {
        match err {
            MeshError::CaveatAmplification => Self::Amplification,
            other => Self::Verification(other.to_string()),
        }
    }
}

/// Host-side: mint a delegated [`AgentKey`] from `parent` with the
/// requested `child_metadata`, then encode its cert chain as a
/// base64-wrapped JSON string ready to drop into the
/// [`AGENT_KEY_ENV`](plugins_protocol::AGENT_KEY_ENV) env var.
///
/// # Errors
///
/// Returns [`EnvelopeError::Amplification`] if
/// `child_metadata.caveats` is not `⊑ parent.cert().metadata.caveats` —
/// [`AgentKey::delegate`] refuses to mint an amplifying child, and we
/// surface that refusal here rather than panicking.
///
/// # Returns
///
/// A base64 string that callers store via
/// [`ProviderPluginBackend::with_agent_key_envelope`](newt_inference::provider_plugin::ProviderPluginBackend::with_agent_key_envelope)
/// or set directly with `Command::env(AGENT_KEY_ENV, &string)`.
pub fn serialize_for_plugin(
    parent: &AgentKey,
    child_metadata: AgentMetadata,
) -> Result<String, EnvelopeError> {
    let child = parent.delegate(child_metadata)?;
    // `serde_json::to_string` for `CertChain` is infallible in practice
    // (the only fields are primitives + maps), but we still propagate
    // any error rather than `expect`-ing — that mirrors the rest of the
    // crate's "no panics from data" stance.
    let json = serde_json::to_string(child.cert())
        .map_err(|e| EnvelopeError::BadCertChain(format!("serialize: {e}")))?;
    Ok(B64.encode(json.as_bytes()))
}

/// Plugin-side: decode the base64-wrapped JSON envelope, verify the cert
/// chain end to end, and convert the verified
/// [`Caveats`](agent_mesh_core::Caveats) into the local
/// [`newt_core::Caveats`] mirror that this workspace's enforcement code
/// (`newt-coder` and friends) consults.
///
/// # Errors
///
/// - [`EnvelopeError::BadBase64`] if the envelope isn't base64.
/// - [`EnvelopeError::BadCertChain`] if the JSON doesn't parse as a
///   [`CertChain`], or the chain's caveats don't round-trip into
///   [`newt_core::Caveats`] (structurally impossible today; the field
///   shapes are identical).
/// - [`EnvelopeError::Verification`] if any signature in the chain
///   fails or the chain is structurally inconsistent.
/// - [`EnvelopeError::Amplification`] if any link in the chain grants
///   more authority than its parent.
///
/// # No trust anchor
///
/// Phase 1c accepts *any* validly-signed chain: the chain self-roots at
/// the user's [`UserPublic`](agent_mesh_core::UserPublic) embedded in
/// the leaf cert, and `verify()` checks the signature against that key.
/// Phase 1d may add an external pin so a forged chain rooting at a
/// *different* user key is rejected even if its self-signatures verify.
pub fn caveats_from_envelope(envelope: &str) -> Result<newt_core::Caveats, EnvelopeError> {
    let bytes = B64
        .decode(envelope)
        .map_err(|e| EnvelopeError::BadBase64(e.to_string()))?;
    let cert: CertChain =
        serde_json::from_slice(&bytes).map_err(|e| EnvelopeError::BadCertChain(e.to_string()))?;
    cert.verify()?;
    // agent_mesh_core::Caveats and newt_core::Caveats are structurally
    // identical (same field names, same serde shape). Bridge via JSON
    // so a future shape divergence surfaces as a deserialize error
    // here, not as a silent semantic skew at enforcement sites.
    let json = serde_json::to_string(&cert.metadata.caveats)
        .map_err(|e| EnvelopeError::BadCertChain(format!("caveats serialize: {e}")))?;
    serde_json::from_str(&json).map_err(|e| {
        EnvelopeError::BadCertChain(format!("caveats not representable in newt-core: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_mesh_core::{Caveats as AmCaveats, CountBound, Scope, UserKey};

    fn fixture_metadata(role: &str, caveats: AmCaveats) -> AgentMetadata {
        AgentMetadata {
            role: role.to_string(),
            host: "test-host".to_string(),
            capabilities: vec!["test".to_string()],
            issued_at: "2026-05-31T00:00:00Z".to_string(),
            expires_at: None,
            caveats,
        }
    }

    /// Happy path: parent (top) → delegate child (attenuated) → encode →
    /// decode → caveats match the child's attenuated authority.
    #[test]
    fn roundtrip_top_to_attenuated() {
        let user = UserKey::generate();
        let parent = AgentKey::issue(&user, fixture_metadata("parent", AmCaveats::top()));

        let child_caveats = AmCaveats {
            exec: Scope::only(["git".to_string(), "cargo".to_string()]),
            net: Scope::none(),
            max_calls: CountBound::AtMost(4),
            ..AmCaveats::top()
        };
        let envelope =
            serialize_for_plugin(&parent, fixture_metadata("child", child_caveats.clone()))
                .expect("delegation must succeed when child ⊑ parent");

        let extracted = caveats_from_envelope(&envelope).expect("envelope must verify");

        // The newt-core mirror's serde shape matches agent-mesh's; round-tripping
        // through JSON should produce the same per-axis decisions.
        assert!(extracted.permits_exec("git"));
        assert!(extracted.permits_exec("cargo"));
        assert!(!extracted.permits_exec("rm"));
        assert!(!extracted.permits_net("openai.com"));
        // max_calls is now bounded.
        assert!(extracted.max_calls.permits_one_more(3));
        assert!(!extracted.max_calls.permits_one_more(4));
    }

    /// Negative: delegate() refuses to mint a child whose caveats amplify
    /// the parent's. The error is surfaced as
    /// [`EnvelopeError::Amplification`], not a panic, so callers can
    /// translate to their own domain error (CoderError::CapabilityDenied,
    /// etc.).
    #[test]
    fn delegate_rejects_amplifying_child() {
        let user = UserKey::generate();
        let parent_caveats = AmCaveats {
            exec: Scope::only(["git".to_string()]),
            ..AmCaveats::top()
        };
        let parent = AgentKey::issue(&user, fixture_metadata("parent", parent_caveats));

        // Child tries to add `rm` to the exec scope — strictly more than parent.
        let child_caveats = AmCaveats {
            exec: Scope::only(["git".to_string(), "rm".to_string()]),
            ..AmCaveats::top()
        };
        let err = serialize_for_plugin(&parent, fixture_metadata("child", child_caveats))
            .expect_err("amplification must be refused");
        assert!(matches!(err, EnvelopeError::Amplification));
    }

    /// A garbage envelope is rejected at the base64 layer with a clear
    /// error — not a panic, not a verification claim that looks like a
    /// signature failure.
    #[test]
    fn rejects_non_base64_envelope() {
        let err =
            caveats_from_envelope("@@@not_base64!!!").expect_err("non-base64 must be rejected");
        assert!(matches!(err, EnvelopeError::BadBase64(_)));
    }

    /// Valid base64 wrapping non-JSON bytes is caught at the cert-parse
    /// layer.
    #[test]
    fn rejects_base64_of_non_json() {
        let env = B64.encode(b"definitely not a cert chain");
        let err = caveats_from_envelope(&env).expect_err("non-JSON must be rejected");
        assert!(matches!(err, EnvelopeError::BadCertChain(_)));
    }

    /// Valid base64'd JSON but tampered after-the-fact: verification
    /// fails. This is the load-bearing invariant — the plugin trusts
    /// the *verified* caveats, never the raw envelope contents.
    #[test]
    fn rejects_tampered_cert_chain() {
        let user = UserKey::generate();
        let agent = AgentKey::issue(&user, fixture_metadata("peer", AmCaveats::top()));
        let mut cert = agent.cert().clone();
        cert.metadata.role = "evil".to_string();
        let env = B64.encode(serde_json::to_vec(&cert).unwrap());
        let err = caveats_from_envelope(&env).expect_err("tampered cert must be rejected");
        assert!(matches!(err, EnvelopeError::Verification(_)));
    }

    /// A delegated (multi-link) chain whose every link is properly
    /// attenuated round-trips through the envelope and produces the
    /// *leaf* caveats — the authority the immediate peer holds, which
    /// is `⊑` every ancestor.
    #[test]
    fn delegated_chain_roundtrips_leaf_caveats() {
        let user = UserKey::generate();
        let parent = AgentKey::issue(
            &user,
            fixture_metadata(
                "parent",
                AmCaveats {
                    exec: Scope::only(["git".to_string(), "cargo".to_string()]),
                    ..AmCaveats::top()
                },
            ),
        );

        let child_caveats = AmCaveats {
            exec: Scope::only(["git".to_string()]),
            ..AmCaveats::top()
        };
        let envelope =
            serialize_for_plugin(&parent, fixture_metadata("child", child_caveats.clone()))
                .expect("attenuating child must serialize");

        let extracted = caveats_from_envelope(&envelope).expect("delegated chain must verify");

        assert!(extracted.permits_exec("git"));
        assert!(!extracted.permits_exec("cargo"));
        assert!(!extracted.permits_exec("rm"));
    }

    /// `From<MeshError>` collapses non-amplification variants into
    /// `Verification` while preserving the dedicated `Amplification` slot.
    /// Mirrors `caveats::CaveatsError`'s mapping so callers handling
    /// either error type get the same shape.
    #[test]
    fn mesh_error_conversion_maps_variants() {
        let amp: EnvelopeError = MeshError::CaveatAmplification.into();
        assert!(matches!(amp, EnvelopeError::Amplification));

        let bad_sig: EnvelopeError = MeshError::BadSignature.into();
        assert!(matches!(bad_sig, EnvelopeError::Verification(_)));
    }
}
