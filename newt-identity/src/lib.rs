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
//! The signed wire `Caveats` ([`agent_mesh_protocol::Caveats`]) and the
//! enforcement-side `Caveats` ([`newt_core::Caveats`]) are deliberate mirrors;
//! [`enforced_caveats`] bridges one to the other via a JSON round-trip, exactly
//! as `newt-mesh`'s plugin envelope does.
//!
//! [1]: ../../docs/decisions/mesh_integration.md

use std::path::{Path, PathBuf};

use agent_mesh_protocol::{AgentKey, AgentMetadata, Caveats as MeshCaveats, MeshError, UserKey};
use newt_core::Caveats;

/// Errors raised while establishing or attenuating a session identity.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// `$HOME` is unset, so `~/.newt/identity.pem` cannot be located.
    #[error("could not locate the home directory for ~/.newt/identity.pem")]
    NoHome,
    /// A key operation (load/save/issue/delegate/verify) failed.
    #[error("identity key error: {0}")]
    Key(String),
    /// The two `Caveats` mirrors failed to round-trip — they have drifted.
    #[error("caveats bridge failed: {0}")]
    Bridge(#[from] serde_json::Error),
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
    AgentKey::issue(user, meta("newt-session-root", MeshCaveats::top()))
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
    let mesh = to_mesh(caveats)?;
    Ok(parent.delegate(meta("newt-session", mesh))?)
}

/// Verify `key`'s cert chain and return the enforcement-side [`Caveats`] it
/// carries. The verification re-checks attenuation at every link, so the
/// returned authority is trustworthy even for a multi-hop delegation.
pub fn enforced_caveats(key: &AgentKey) -> Result<Caveats, IdentityError> {
    key.cert().verify()?;
    from_mesh(&key.cert().metadata.caveats)
}

/// Bridge enforcement-side `Caveats` → the signed wire type via a JSON
/// round-trip. The two are structural mirrors; a failure here means they have
/// drifted (guarded by `bridge_roundtrips_every_preset`).
fn to_mesh(c: &Caveats) -> Result<MeshCaveats, IdentityError> {
    let json = serde_json::to_string(c)?;
    Ok(serde_json::from_str(&json)?)
}

/// Bridge the signed wire `Caveats` → the enforcement-side type.
fn from_mesh(c: &MeshCaveats) -> Result<Caveats, IdentityError> {
    let json = serde_json::to_string(c)?;
    Ok(serde_json::from_str(&json)?)
}

/// Build the signed metadata for a session key. `issued_at` is a *claim* in a
/// signed cert (never a coordination primitive), so wall-clock is appropriate.
fn meta(role: &str, caveats: MeshCaveats) -> AgentMetadata {
    AgentMetadata {
        role: role.to_string(),
        host: "local".to_string(),
        capabilities: Vec::new(),
        issued_at: chrono::Utc::now().to_rfc3339(),
        expires_at: None,
        caveats,
    }
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
    fn bridge_roundtrips_every_preset() {
        // Guards against the newt-core / agent-mesh `Caveats` mirrors drifting.
        for p in [
            PermissionPreset::ReadOnly,
            PermissionPreset::WorkspaceEdit,
            PermissionPreset::WorkspaceDev,
            PermissionPreset::FullAccess,
        ] {
            let label = format!("{p:?}");
            let c = preset_caveats(p);
            let back = from_mesh(&to_mesh(&c).unwrap()).unwrap();
            assert_eq!(c, back, "preset {label} must survive the caveats bridge");
        }
    }
}
