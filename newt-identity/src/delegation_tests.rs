use super::*;
use crate::{delegate_for_plugin, session_root, UserKey};

fn envelope(parent: &crate::AgentKey) -> String {
    delegate_for_plugin(
        parent,
        "newt-helper",
        Caveats {
            fs_read: newt_core::Scope::only(["workspace".to_string()]),
            fs_write: newt_core::Scope::none(),
            exec: newt_core::Scope::none(),
            net: newt_core::Scope::none(),
            ..Caveats::top()
        },
    )
    .unwrap()
}

#[test]
fn delegated_receiver_accepts_the_exact_fresh_child() {
    let parent = session_root(&UserKey::generate());
    let envelope = envelope(&parent);
    let expected = RawContentId::from_content(envelope.as_bytes());
    let verified = verify_delegation(envelope.as_bytes(), &expected).unwrap();
    assert_eq!(verified.caveats().exec, newt_core::Scope::none());
    assert_eq!(verified.caveats().fs_write, newt_core::Scope::none());
    assert_eq!(
        verified.caveats().fs_read,
        newt_core::Scope::only(["workspace".to_string()])
    );
}

/// Generation is a causal revocation scope, not the `expires_at` wall-clock
/// metadata. A matching content anchor supplies no trusted current generation.
#[test]
fn delegated_receiver_requires_trusted_context_for_generation_scoped_chains() {
    let generation = 7;
    let scoped = Caveats {
        valid_for_generation: newt_core::Scope::only([generation]),
        ..Caveats::top()
    };
    for bounded_parent in [false, true] {
        let root = session_root(&UserKey::generate());
        let parent = if bounded_parent {
            root.delegate(crate::plugin_child_metadata("parent", scoped.clone()))
                .unwrap()
        } else {
            root
        };
        // In the second case both parent and child carry the same allowed
        // generation; the existing cert walk validates the entire chain.
        parent.cert().verify_at(generation).unwrap();
        let body = delegate_for_plugin(&parent, "newt-helper", scoped.clone()).unwrap();
        let cert: CertChain = serde_json::from_slice(&B64.decode(&body).unwrap()).unwrap();
        cert.verify_at(generation).unwrap();
        assert!(cert.verify_at(generation + 1).is_err());
        assert!(
            matches!(
                verify_delegation(body.as_bytes(), &RawContentId::from_content(body.as_bytes())),
                Err(DelegationError::InvalidCertificate)
            ),
            "a generation-scoped chain needs trusted generation context (bounded parent: {bounded_parent})"
        );
    }
}

#[test]
fn delegated_receiver_rejects_an_unrelated_exact_content_id() {
    let parent = session_root(&UserKey::generate());
    assert!(matches!(
        verify_delegation(
            envelope(&parent).as_bytes(),
            &RawContentId::from_content(b"a different approved launch")
        ),
        Err(DelegationError::UnexpectedEnvelope)
    ));
}

#[test]
fn delegated_receiver_rejects_other_valid_roots_and_sibling_children() {
    let parent = session_root(&UserKey::generate());
    let approved = envelope(&parent);
    let expected = RawContentId::from_content(approved.as_bytes());
    for substituted in [
        envelope(&parent),
        envelope(&session_root(&UserKey::generate())),
    ] {
        assert!(matches!(
            verify_delegation(substituted.as_bytes(), &expected),
            Err(DelegationError::UnexpectedEnvelope)
        ));
    }
}

#[test]
fn delegated_receiver_pins_exact_envelope_bytes_not_equivalent_certificate_json() {
    let parent = session_root(&UserKey::generate());
    let original = envelope(&parent);
    let expected = RawContentId::from_content(original.as_bytes());
    let cert: CertChain = serde_json::from_slice(&B64.decode(&original).unwrap()).unwrap();
    let reencoded = B64.encode(serde_json::to_vec_pretty(&cert).unwrap());
    assert_ne!(original.as_bytes(), reencoded.as_bytes());

    // Whitespace changes the payload identity, not the certificate or signature.
    let reencoded_id = RawContentId::from_content(reencoded.as_bytes());
    assert_ne!(expected, reencoded_id);
    let verified = verify_delegation(reencoded.as_bytes(), &reencoded_id).unwrap();
    assert_eq!(verified.caveats(), &cert.metadata.caveats);
    assert!(matches!(
        verify_delegation(reencoded.as_bytes(), &expected),
        Err(DelegationError::UnexpectedEnvelope)
    ));
}

#[test]
fn delegated_receiver_rejects_missing_and_malformed_envelopes() {
    for body in [b"".as_slice(), b"not base64", b"e30="] {
        assert!(matches!(
            verify_delegation(body, &RawContentId::from_content(body)),
            Err(DelegationError::InvalidEnvelope)
        ));
    }
}

#[test]
fn delegated_receiver_checks_signatures_even_when_bytes_match_the_anchor() {
    let parent = session_root(&UserKey::generate());
    let original = envelope(&parent);
    let cert: CertChain = serde_json::from_slice(&B64.decode(original).unwrap()).unwrap();
    for changed in [
        {
            let mut leaf = cert.clone();
            leaf.metadata.caveats.fs_write = newt_core::Scope::All;
            leaf
        },
        {
            let mut leaf = cert;
            leaf.agent_pubkey[0] ^= 1;
            leaf
        },
    ] {
        let body = B64.encode(serde_json::to_vec(&changed).unwrap());
        assert!(matches!(
            verify_delegation(
                body.as_bytes(),
                &RawContentId::from_content(body.as_bytes())
            ),
            Err(DelegationError::InvalidCertificate)
        ));
    }
}
