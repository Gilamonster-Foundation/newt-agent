//! Issue #93: the Coder's `parent_key` threading produces a
//! plugin-side envelope whose cert chain roots back to the operator's
//! `UserKey` from `~/.newt/identity.pem` — never a synthetic key
//! generated at spawn time.
//!
//! This integration test models the real headless dispatch shape:
//!   1. The operator's `UserKey` lives on disk.
//!   2. `WorkerIdentity::Operator` wraps `Arc<AgentKey>` (session root).
//!   3. The coder is configured with that root via `with_parent_key`.
//!   4. When a subprocess plugin spawn would happen, the coder mints
//!      a delegated child via `plugin_envelope_for`, and the resulting
//!      envelope walks back to the operator.
//!
//! Without this test, a future refactor could silently insert a
//! `UserKey::generate()` at the plugin spawn site and break the
//! chain-rooting invariant. The matching `no_synthetic_keys.rs`
//! scanner in `newt-acp-worker/tests/` catches the *source-text*
//! regression; this test catches the *semantic* regression even if the
//! source happened to look clean.

use std::sync::Arc;

use base64::Engine;
use newt_coder::Coder;
use newt_core::router::Tier;
use newt_core::Caveats;
use newt_inference::backend::{ChatReply, ChatRequest, InferenceBackend};
use tempfile::TempDir;

/// Tiny inert backend — the parent-key tests don't actually call
/// `complete()`, only the envelope-mint chokepoint.
struct InertBackend;

#[async_trait::async_trait]
impl InferenceBackend for InertBackend {
    fn name(&self) -> &str {
        "inert"
    }
    fn model_id(&self) -> &str {
        "inert-model"
    }
    fn supports_tier(&self, _t: Tier) -> bool {
        false
    }
    async fn complete(&self, _req: ChatRequest) -> anyhow::Result<ChatReply> {
        unreachable!("envelope-mint tests must not call complete()")
    }
}

#[test]
fn coder_plugin_envelope_chain_roots_at_operator_userkey() {
    let dir = TempDir::new().unwrap();
    let key_path = dir.path().join("identity.pem");
    // Operator key — written to disk like ~/.newt/identity.pem.
    let user = newt_identity::load_or_generate(&key_path).unwrap();
    let user_fp = user.fingerprint();
    // WorkerIdentity::Operator { root } shape: an Arc<AgentKey> minted
    // from the operator user.
    let root = Arc::new(newt_identity::session_root(&user));

    let backend: Arc<dyn InferenceBackend> = Arc::new(InertBackend);
    let coder = Coder::new(backend).with_parent_key(Arc::clone(&root));
    assert!(
        coder.parent_key().is_some(),
        "Coder must hold the threaded parent key"
    );

    let plugin_caveats = Caveats::top();
    let envelope = coder
        .plugin_envelope_for("openai-provider", plugin_caveats)
        .expect("parent key configured → envelope path is available")
        .expect("delegation must succeed under top caveats");

    // The envelope is base64-encoded JSON CertChain. Decode + verify +
    // walk the chain to the operator.
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&envelope)
        .unwrap();
    let leaf: agent_mesh_protocol::CertChain = serde_json::from_slice(&bytes).unwrap();
    leaf.verify().expect("cert chain must verify");
    assert_eq!(
        leaf.user_fingerprint(),
        user_fp,
        "plugin envelope must chain back to operator UserKey"
    );
}

#[test]
fn coder_without_parent_key_returns_none_no_synthetic_fallback() {
    // The AllowNoKey debug fallback path — Coder has no parent key.
    // The envelope-mint chokepoint MUST return None rather than
    // manufacturing a fresh UserKey/AgentKey. This is the documented
    // behavior the no_synthetic_keys.rs scanner protects against in
    // the source-text dimension; this test pins the runtime
    // counterpart.
    let backend: Arc<dyn InferenceBackend> = Arc::new(InertBackend);
    let coder = Coder::new(backend);
    assert!(
        coder.parent_key().is_none(),
        "Coder::new without with_parent_key has no parent"
    );
    assert!(
        coder
            .plugin_envelope_for("plugin", Caveats::top())
            .is_none(),
        "no parent key → no envelope (no synthetic-key fallback, issue #93)"
    );
}

#[test]
fn coder_plugin_envelope_refuses_amplification() {
    // The Coder holds a narrowed parent; a caller asking the plugin to
    // run with strictly wider authority is refused at the delegate()
    // boundary, surfaced as EnvelopeError::Amplification.
    let dir = TempDir::new().unwrap();
    let user = newt_identity::load_or_generate(&dir.path().join("identity.pem")).unwrap();
    let session = newt_identity::session_root(&user);
    let narrow = Caveats {
        exec: newt_core::Scope::none(),
        ..Caveats::top()
    };
    let worker = newt_identity::attenuate(&session, &narrow).unwrap();

    let backend: Arc<dyn InferenceBackend> = Arc::new(InertBackend);
    let coder = Coder::new(backend).with_parent_key(Arc::new(worker));

    let amplifying = Caveats::top(); // exec = All > parent's None.
    let err = coder
        .plugin_envelope_for("evil-plugin", amplifying)
        .expect("parent set → envelope path attempted")
        .expect_err("amplification must be refused");
    assert!(
        matches!(err, newt_identity::EnvelopeError::Amplification),
        "expected Amplification, got {err:?}"
    );
}

#[test]
fn coder_plugin_envelope_is_attenuation_only_subset_of_parent() {
    // The minted envelope's leaf caveats must be `⊑` the parent's,
    // by construction (delegate's signed-and-verified attenuation
    // check). Pin that property: a request with strictly-narrower
    // caveats reaches the plugin with exactly those caveats, NOT the
    // parent's wider authority.
    let dir = TempDir::new().unwrap();
    let user = newt_identity::load_or_generate(&dir.path().join("identity.pem")).unwrap();
    let root = newt_identity::session_root(&user); // top
    let backend: Arc<dyn InferenceBackend> = Arc::new(InertBackend);
    let coder = Coder::new(backend).with_parent_key(Arc::new(root));

    let plugin_caveats = Caveats {
        exec: newt_core::Scope::only(["git".to_string()]),
        fs_write: newt_core::Scope::none(),
        ..Caveats::top()
    };
    let envelope = coder
        .plugin_envelope_for("attenuated-plugin", plugin_caveats.clone())
        .unwrap()
        .unwrap();

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&envelope)
        .unwrap();
    let leaf: agent_mesh_protocol::CertChain = serde_json::from_slice(&bytes).unwrap();
    leaf.verify().unwrap();
    assert_eq!(
        leaf.metadata.caveats, plugin_caveats,
        "plugin's leaf authority must equal the requested attenuated caveats"
    );
}
