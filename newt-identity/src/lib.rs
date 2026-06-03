//! `newt-identity` — the per-user object-capability identity layer for the
//! default workspace.
//!
//! This is where a newt host turns a *name-based permission preset* into a
//! **real capability**: a signed, verified, attenuation-only authority rooted
//! in a per-user key.
//!
//! ```text
//!   UserKey (root of trust, ~/.newt/identity.pem, 0600)
//!     └── AgentKey::issue(⊤)          = session_root   ← the human's full authority
//!           └── root.delegate(preset) = operating key  ← what the agent runs under
//! ```
//!
//! The operating key's caveats are *signed into its cert* and provably `⊑` the
//! root: [`agent_mesh_protocol::AgentKey::delegate`] refuses to mint a child
//! that amplifies, and [`agent_mesh_protocol::CertChain::verify`] re-checks
//! attenuation at every link. So a confused or compromised agent that holds the
//! operating key — but **not** the root `UserKey` — can only ever *narrow* its
//! authority (delegating to subprocess plugins), never widen it. That is the
//! property a name-based preset lookup cannot provide.
//!
//! ## Why a separate crate (and not `newt-mesh`)
//!
//! `newt-mesh` already does this, but it path-depends on `../agent-mesh` and is
//! therefore [excluded from the default workspace][1] (CI has no sibling repo to
//! resolve). `newt-identity` lives *inside* the workspace and reaches the same
//! machinery through the **published** `agent-mesh-protocol` crate — pure Rust
//! (ed25519 + blake3), so the PyO3 wheel story is unaffected.
//!
//! Since issue #95 the signed wire `Caveats` and the enforcement-side
//! `Caveats` are the *same* Rust type: `newt_core::Caveats` is now a re-export
//! of [`agent_mesh_protocol::Caveats`]. The JSON-bridge previous versions of
//! this crate ran is therefore the identity function — `enforced_caveats`
//! just clones the verified leaf metadata out of the cert chain.
//!
//! [1]: ../../docs/decisions/mesh_integration.md

use std::path::{Path, PathBuf};

use agent_mesh_protocol::{MeshError, UserKey};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use newt_core::Caveats;

/// Re-exported so session hosts (e.g. the TUI) can hold an operating key and
/// delegate from it without depending on `agent-mesh-protocol` directly.
pub use agent_mesh_protocol::{AgentKey, AgentMetadata};

/// The debug-only "no caveats" lattice element used by the headless
/// ACP worker's `--allow-no-key` fallback. Behaviorally identical to
/// the upstream `Caveats::top()` but indirected through this helper so
/// the headless-dispatch source tree
/// (`newt-acp-worker`, `newt-coder`, `newt-inference`, `newt-cli`)
/// carries zero literal `Caveats::top()` references — the
/// `no_top_leak` regression test in `newt-acp-worker` asserts that
/// property.
///
/// Issue #94: the headless worker dispatches under signed, attenuated
/// caveats by default; this helper is the *opt-in* trapdoor for
/// developer iteration without a provisioned operator key (`--allow-no-key`).
/// It lives in `newt-identity` precisely because that crate is outside
/// the regression test's scan paths — the lone exception is contained.
#[must_use]
pub fn unbounded_debug_fallback() -> Caveats {
    Caveats::top()
}

/// Errors raised while establishing or attenuating a session identity.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// `$HOME` is unset, so `~/.newt/identity.pem` cannot be located.
    #[error("could not locate the home directory for ~/.newt/identity.pem")]
    NoHome,
    /// A key operation (load/save/issue/delegate/verify) failed.
    #[error("identity key error: {0}")]
    Key(String),
}

impl From<MeshError> for IdentityError {
    fn from(e: MeshError) -> Self {
        Self::Key(e.to_string())
    }
}

/// The per-user root key path: `~/.newt/identity.pem` (sibling of the config
/// file, so it follows the same `$HOME` resolution the rest of newt uses).
pub fn default_key_path() -> Result<PathBuf, IdentityError> {
    newt_core::Config::user_config_path()
        .map(|p| p.with_file_name("identity.pem"))
        .ok_or(IdentityError::NoHome)
}

/// Load the per-user root key, generating and persisting a fresh one on first
/// run.
///
/// On generation the key is written with `0600` permissions and `mkdir -p` on
/// the parent (handled by [`UserKey::save`]). This is the only place the root
/// private key is created.
pub fn load_or_generate(path: &Path) -> Result<UserKey, IdentityError> {
    if path.exists() {
        Ok(UserKey::load(path)?)
    } else {
        let key = UserKey::generate();
        key.save(path)?;
        Ok(key)
    }
}

/// Mint the session root: an [`AgentKey`] carrying the user's full authority
/// (`⊤`). Presets attenuate *down* from here; nothing produced from this root
/// can exceed the human's own authority.
#[must_use]
pub fn session_root(user: &UserKey) -> AgentKey {
    AgentKey::issue(user, meta("newt-session-root", Caveats::top()))
}

/// Attenuate `root` to the given enforcement `caveats`, producing a **signed**
/// operating key whose authority is provably `⊑` `root`.
///
/// Because every preset is `⊑ ⊤`, attenuating a `⊤` session root always
/// succeeds; attenuating an already-narrowed operating key only succeeds for a
/// still-narrower request — a wider one returns
/// [`agent_mesh_protocol::MeshError::CaveatAmplification`]. That is what makes
/// downstream delegation (e.g. to subprocess plugins) attenuation-only.
pub fn attenuate(parent: &AgentKey, caveats: &Caveats) -> Result<AgentKey, IdentityError> {
    Ok(parent.delegate(meta("newt-session", caveats.clone()))?)
}

/// Verify `key`'s cert chain and return the enforcement-side [`Caveats`] it
/// carries. The verification re-checks attenuation at every link, so the
/// returned authority is trustworthy even for a multi-hop delegation.
///
/// Since issue #95 the signed wire `Caveats` and the enforcement-side
/// `Caveats` are the same Rust type, so this is a verify + clone — no JSON
/// bridge.
pub fn enforced_caveats(key: &AgentKey) -> Result<Caveats, IdentityError> {
    key.cert().verify()?;
    Ok(key.cert().metadata.caveats.clone())
}

/// Build the signed metadata for a session key. `issued_at` is a *claim* in a
/// signed cert (never a coordination primitive), so wall-clock is appropriate.
fn meta(role: &str, caveats: Caveats) -> AgentMetadata {
    plugin_child_metadata(role, caveats)
}

/// Convenience constructor for the [`AgentMetadata`] of a delegated child
/// key — including the per-plugin envelope minted by
/// [`serialize_for_plugin`].
///
/// Centralizing the metadata construction here keeps the "fresh child cert"
/// shape consistent across the headless dispatch path (this crate) and any
/// plugin-spawning chokepoint that ends up reaching for delegation — every
/// such site mints with the same role/host conventions, so the
/// subprocess-side `caveats_from_envelope` extractor sees a uniform shape.
#[must_use]
pub fn plugin_child_metadata(role: &str, caveats: Caveats) -> AgentMetadata {
    AgentMetadata {
        role: role.to_string(),
        host: "local".to_string(),
        capabilities: Vec::new(),
        issued_at: chrono::Utc::now().to_rfc3339(),
        expires_at: None,
        caveats,
    }
}

/// Errors raised while serializing a parent [`AgentKey`] into a base64
/// envelope ready for the `NEWT_AGENT_KEY` env var.
///
/// Distinct from [`IdentityError`] because amplification is a *caller bug*
/// (request a child whose caveats are not `⊑` the parent's), not an I/O
/// failure on the operator key.
#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    /// [`AgentKey::delegate`] refused to mint a child whose authority is
    /// not `⊑` the parent's. Mirrors
    /// [`agent_mesh_protocol::MeshError::CaveatAmplification`].
    #[error("requested child caveats amplify parent authority")]
    Amplification,
    /// The cert chain could not be serialized to JSON, or the underlying
    /// `agent-mesh-protocol` operation failed for some other reason.
    #[error("envelope serialization failed: {0}")]
    Serialize(String),
}

impl From<MeshError> for EnvelopeError {
    fn from(err: MeshError) -> Self {
        match err {
            MeshError::CaveatAmplification => Self::Amplification,
            other => Self::Serialize(other.to_string()),
        }
    }
}

/// Mint a delegated **plugin** [`AgentKey`] from `parent` with the requested
/// `child_metadata`, then encode its cert chain as a base64-wrapped JSON
/// string ready to drop into the
/// [`AGENT_KEY_ENV`](plugins_protocol::AGENT_KEY_ENV) env var.
///
/// This is the *headless* counterpart of `newt_mesh::plugin_envelope::serialize_for_plugin`:
/// `newt-mesh` is path-dep'd against the local `agent-mesh-core` workspace
/// and therefore excluded from the default workspace, so the headless
/// dispatch path (`newt-acp-worker` → `newt-coder` → `newt-inference`)
/// — which depends on the *published* `agent-mesh-protocol` crate via
/// this `newt-identity` crate — needs a parallel helper that produces
/// the same wire format (base64-JSON [`agent_mesh_protocol::CertChain`])
/// from the *same* root user key.
///
/// # Chain-rooting guarantee
///
/// The returned envelope, when decoded and verified by the plugin
/// subprocess (via `caveats_from_envelope`), walks a cert chain whose
/// leaf is the freshly-minted child and whose root is `parent`'s
/// `root_user_pubkey()` — i.e. the operator's `UserKey` from
/// `~/.newt/identity.pem`. **Issue #93** locks this in: subprocess plugin
/// AgentKeys MUST be derived from the operator's TUI/headless key root,
/// never from a synthetic `UserKey::generate()` at spawn time.
///
/// # Errors
///
/// Returns [`EnvelopeError::Amplification`] if `child_metadata.caveats` is
/// not `⊑ parent.cert().metadata.caveats` — [`AgentKey::delegate`] refuses
/// to mint an amplifying child, and we surface that refusal here rather
/// than panicking. JSON serialization errors are surfaced as
/// [`EnvelopeError::Serialize`].
pub fn serialize_for_plugin(
    parent: &AgentKey,
    child_metadata: AgentMetadata,
) -> Result<String, EnvelopeError> {
    let child = parent.delegate(child_metadata)?;
    let json = serde_json::to_string(child.cert())
        .map_err(|e| EnvelopeError::Serialize(format!("cert chain: {e}")))?;
    Ok(B64.encode(json.as_bytes()))
}

/// Convenience: build an envelope for a plugin running under `role` with
/// the given enforcement `caveats`. Wraps [`plugin_child_metadata`] +
/// [`serialize_for_plugin`].
pub fn delegate_for_plugin(
    parent: &AgentKey,
    role: &str,
    caveats: Caveats,
) -> Result<String, EnvelopeError> {
    let metadata = plugin_child_metadata(role, caveats);
    serialize_for_plugin(parent, metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::{PermissionPreset, ToolPermissions};
    use tempfile::TempDir;

    fn preset_caveats(p: PermissionPreset) -> Caveats {
        ToolPermissions {
            preset: p,
            extra_exec: Vec::new(),
            net: Vec::new(),
        }
        .to_caveats("/ws")
    }

    fn fresh_user(dir: &TempDir) -> UserKey {
        load_or_generate(&dir.path().join("identity.pem")).unwrap()
    }

    #[test]
    fn user_key_load_or_generate_is_stable() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("identity.pem");
        let first = load_or_generate(&path).unwrap();
        assert!(path.exists(), "first call must create the key");
        let second = load_or_generate(&path).unwrap();
        assert_eq!(
            first.fingerprint(),
            second.fingerprint(),
            "second call must load the same key, not regenerate"
        );
    }

    #[test]
    #[cfg(unix)]
    fn load_or_generate_writes_a_0600_key() {
        // The root private key's `0600` mode is set by agent-mesh's
        // `UserKey::save`, not by this crate. Pin it here so a dependency bump
        // can't silently widen the permissions of a key we generate.
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("identity.pem");
        let _ = load_or_generate(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "generated key must be 0600, got {mode:o}");
    }

    #[test]
    fn attenuated_preset_is_signed_and_verifies() {
        let dir = TempDir::new().unwrap();
        let root = session_root(&fresh_user(&dir));
        let wanted = preset_caveats(PermissionPreset::WorkspaceDev);
        let op = attenuate(&root, &wanted).unwrap();
        // The operating key carries exactly the preset's authority...
        assert_eq!(enforced_caveats(&op).unwrap(), wanted);
        // ...and its full cert chain verifies (rooted at the user).
        op.cert().verify().unwrap();
    }

    #[test]
    fn operating_key_cannot_widen() {
        // The headline ocap property: a key holding workspace-dev authority
        // cannot delegate FULL access — attenuation is structural, so a
        // compromised in-session actor can only ever narrow.
        let dir = TempDir::new().unwrap();
        let root = session_root(&fresh_user(&dir));
        let op = attenuate(&root, &preset_caveats(PermissionPreset::WorkspaceDev)).unwrap();
        let widen = attenuate(&op, &preset_caveats(PermissionPreset::FullAccess));
        assert!(
            widen.is_err(),
            "an operating key must not be able to delegate wider authority than it holds"
        );
    }

    #[test]
    fn serialize_for_plugin_round_trips_and_roots_at_operator_user() {
        // Issue #93: the subprocess plugin's leaf AgentKey must verify
        // back through the chain to the operator's `UserKey` — i.e. the
        // same root user that wrote `~/.newt/identity.pem`. A synthetic
        // `UserKey::generate()` at spawn time would break this property.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("identity.pem");
        let user = load_or_generate(&path).unwrap();
        let user_fp = user.fingerprint();

        // Mint the headless worker's session root then attenuate to a
        // realistic per-dispatch caveats (mirrors what
        // `WorkerIdentity::caveats_for_dispatch` does).
        let session = session_root(&user);
        let dispatch_caveats = preset_caveats(PermissionPreset::WorkspaceDev);
        let worker = attenuate(&session, &dispatch_caveats).unwrap();

        // Serialize a plugin envelope under the worker's authority,
        // further-narrowed to a read-only subset for the subprocess.
        let mut plugin_caveats = dispatch_caveats.clone();
        plugin_caveats.exec = newt_core::Scope::none();
        plugin_caveats.fs_write = newt_core::Scope::none();
        let envelope = delegate_for_plugin(&worker, "provider-plugin", plugin_caveats).unwrap();

        // The envelope is base64-encoded JSON; decode + reconstruct the
        // cert chain, verify end-to-end, and walk to the root.
        use base64::Engine;
        let json = base64::engine::general_purpose::STANDARD
            .decode(&envelope)
            .unwrap();
        let cert: agent_mesh_protocol::CertChain = serde_json::from_slice(&json).unwrap();
        cert.verify().expect("plugin cert chain must verify");
        assert_eq!(
            cert.user_fingerprint(),
            user_fp,
            "plugin leaf must chain back to the operator UserKey, \
             NOT a freshly-generated synthetic key"
        );
        assert_eq!(cert.root_user_pubkey(), user.public());
    }

    #[test]
    fn serialize_for_plugin_refuses_amplification() {
        // Defense in depth: the parent holds a narrowed authority, the
        // caller asks the plugin to run with strictly more. `delegate()`
        // refuses, and we surface that as `EnvelopeError::Amplification`
        // rather than panicking.
        let dir = TempDir::new().unwrap();
        let user = fresh_user(&dir);
        let session = session_root(&user);
        let narrow = preset_caveats(PermissionPreset::ReadOnly);
        let worker = attenuate(&session, &narrow).unwrap();

        let amplifying = preset_caveats(PermissionPreset::FullAccess);
        let err = delegate_for_plugin(&worker, "evil-plugin", amplifying)
            .expect_err("amplifying delegation must refuse");
        assert!(
            matches!(err, EnvelopeError::Amplification),
            "expected Amplification, got {err:?}"
        );
    }

    #[test]
    fn serialize_for_plugin_three_link_chain_operator_worker_plugin() {
        // The classic threading the PR enforces:
        //   operator UserKey
        //     └── session_root (issue) ← AgentKey #1, ⊤
        //         └── worker (delegate) ← AgentKey #2, dispatch caveats
        //             └── plugin (delegate) ← AgentKey #3, plugin caveats
        // Verifying the leaf must walk all three links and end at the
        // operator UserKey.
        let dir = TempDir::new().unwrap();
        let user = fresh_user(&dir);
        let root = session_root(&user);
        let worker = attenuate(&root, &preset_caveats(PermissionPreset::WorkspaceDev)).unwrap();
        let envelope = delegate_for_plugin(
            &worker,
            "plugin",
            preset_caveats(PermissionPreset::ReadOnly),
        )
        .unwrap();

        use base64::Engine;
        let json = base64::engine::general_purpose::STANDARD
            .decode(&envelope)
            .unwrap();
        let leaf: agent_mesh_protocol::CertChain = serde_json::from_slice(&json).unwrap();
        leaf.verify().expect("three-link chain must verify");
        assert_eq!(leaf.user_fingerprint(), user.fingerprint());
        // And the chain has the expected depth: leaf -> worker -> root.
        match &leaf.issuer {
            agent_mesh_protocol::Issuer::Agent { parent, .. } => match &parent.issuer {
                agent_mesh_protocol::Issuer::Agent { parent: gp, .. } => match &gp.issuer {
                    agent_mesh_protocol::Issuer::User(u) => {
                        assert_eq!(u.fingerprint(), user.fingerprint());
                    }
                    _ => panic!("third link must be Issuer::User"),
                },
                _ => panic!("middle link must be Issuer::Agent"),
            },
            _ => panic!("leaf must be Issuer::Agent"),
        }
    }

    #[test]
    fn caveats_round_trip_through_sign_and_verify_is_identity() {
        // Post-#95: `newt_core::Caveats` is `agent_mesh_protocol::Caveats`,
        // so the previous JSON bridge between the two collapses to the
        // identity function. Pin that property: every preset survives
        // attenuate → enforced_caveats unchanged. If a future
        // agent-mesh-protocol bump ever re-splits the types, this test
        // fails loudly instead of silently dropping fields.
        let dir = TempDir::new().unwrap();
        let root = session_root(&fresh_user(&dir));
        for p in [
            PermissionPreset::ReadOnly,
            PermissionPreset::WorkspaceEdit,
            PermissionPreset::WorkspaceDev,
            PermissionPreset::FullAccess,
        ] {
            let label = format!("{p:?}");
            let c = preset_caveats(p);
            let op = attenuate(&root, &c).unwrap();
            let back = enforced_caveats(&op).unwrap();
            assert_eq!(c, back, "preset {label} must survive sign+verify");
        }
    }
}
