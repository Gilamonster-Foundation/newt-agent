//! Workspace-bound, signed and encrypted exact permission approvals.
//!
//! The terminal supplies the reviewed snapshot and trusted operator keys. This
//! store never projects remote-tool or git grants onto another capability, and
//! never generates keys. The permission gate still enforces denials, danger,
//! preset floors, and inherited delegation before using an approval.
//!
//! Only ciphertext is staged or written. Removing the file removes approvals
//! on the next reload; running sessions must reload or restart. Signatures and
//! encryption do not prevent restoration of an older valid ciphertext, and
//! they do not protect against an actor holding the operator's private keys.

// Model: GPT-6 | Harness: Codex | Operator: Shawn Hartsock | Time: 13:49 EDT | Date: 2026-09-16

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use agent_mesh_protocol::{SerdeSig, UserKey, UserPublic};
use anyhow::{ensure, Context as _};
use content_addressable::{canonical, ContentAddressable, ContentError, ContentId};
use serde::{Deserialize, Serialize};

use crate::secrets::TokenIdentity;
use crate::DenialKind;

const SCHEMA: &str = "newt.durable-grants/v1";
// Limits apply before decryption/decoding and before a replacement is staged.
const MAX_STORE_BYTES: usize = 1024 * 1024;
const MAX_GRANTS: usize = 4096;
const MAX_TARGET_BYTES: usize = 4096;
const MAX_WORKSPACES: usize = 256;

/// Exact capability-kind and target pairs; no wildcard or path-prefix matching.
pub type GrantSet = BTreeSet<(DenialKind, String)>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    schema: String,
    workspaces: BTreeMap<String, GrantSet>,
}

impl Payload {
    fn empty() -> Self {
        Self {
            schema: SCHEMA.into(),
            workspaces: BTreeMap::new(),
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.schema == SCHEMA,
            "unsupported durable permission schema"
        );
        ensure!(
            self.workspaces.len() <= MAX_WORKSPACES,
            "too many permission workspaces"
        );
        ensure!(
            self.workspaces.values().map(GrantSet::len).sum::<usize>() <= MAX_GRANTS,
            "too many durable permission grants"
        );
        for (workspace, grants) in &self.workspaces {
            validate_target(workspace)?;
            ensure!(
                Path::new(workspace).is_absolute(),
                "permission workspace must be absolute"
            );
            validate_grants(grants)?;
        }
        Ok(())
    }
}

impl ContentAddressable for Payload {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        canonical::to_canonical_dagcbor(self)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignedPayload {
    payload: Payload,
    signature: SerdeSig,
}

impl ContentAddressable for SignedPayload {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        canonical::to_canonical_dagcbor(self)
    }
}

/// Approvals verified against caller-supplied keys for one canonical workspace.
pub struct VerifiedGrants {
    grants: GrantSet,
    id: ContentId,
}

impl VerifiedGrants {
    pub fn grants(&self) -> &GrantSet {
        &self.grants
    }

    pub fn content_id(&self) -> &ContentId {
        &self.id
    }
}

/// The encrypted store beside the selected configuration file.
pub fn store_path(config_path: &Path) -> PathBuf {
    config_path
        .with_file_name("ocap")
        .join("session-grants.age")
}

/// Load verified approvals; only a missing store is an empty result.
///
/// Requires existing keys. No fallback to plaintext or unverified approvals is
/// permitted. The encrypted file and its decoded bytes are each limited to
/// 1 MiB; the store holds at most 4096 grants across 256 workspaces.
pub fn load(
    path: &Path,
    workspace: &Path,
    trusted_root: &UserPublic,
    encryption: &TokenIdentity,
) -> anyhow::Result<VerifiedGrants> {
    let workspace = workspace_binding(workspace)?;
    verified_for(read_payload(path, trusted_root, encryption)?, &workspace)
}

/// Verify, merge and atomically save the reviewed snapshot under one file lock.
///
/// Existing data must verify before any additions are signed. Other workspaces
/// and concurrent acknowledged additions are retained. A successful return
/// means the saved ciphertext was read back and verified. An atomic filesystem
/// error retains its commit-status information; an error after rename does not
/// imply that the replacement was rolled back.
pub fn merge(
    path: &Path,
    workspace: &Path,
    additions: &GrantSet,
    root: &UserKey,
    encryption: &TokenIdentity,
) -> anyhow::Result<VerifiedGrants> {
    validate_grants(additions)?;
    if additions.is_empty() {
        return load(path, workspace, &root.public(), encryption);
    }
    let workspace = workspace_binding(workspace)?;
    let destination = crate::atomic_fs::ResolvedPath::resolve(path)?;
    let _lock = crate::atomic_fs::acquire_lock(&destination.lock_path())?;
    let mut payload = read_payload(destination.as_path(), &root.public(), encryption)?;
    payload
        .workspaces
        .entry(workspace.clone())
        .or_default()
        .extend(additions.iter().cloned());
    let ciphertext = encode(&payload, root, encryption)?;
    let expected = payload.content_id()?;
    destination.atomic_write_private(ciphertext.as_bytes())?;
    let saved = read_payload(destination.as_path(), &root.public(), encryption)?;
    saved.ensure_content_id(&expected)?;
    verified_for(saved, &workspace)
}

fn validate_target(target: &str) -> anyhow::Result<()> {
    ensure!(
        !target.trim().is_empty() && !target.contains('\0') && target.len() <= MAX_TARGET_BYTES,
        "permission target must be nonempty, NUL-free and at most 4096 bytes"
    );
    Ok(())
}

fn validate_grants(grants: &GrantSet) -> anyhow::Result<()> {
    ensure!(
        grants.len() <= MAX_GRANTS,
        "too many durable permission grants"
    );
    for (_, target) in grants {
        validate_target(target)?;
    }
    Ok(())
}

fn workspace_binding(workspace: &Path) -> anyhow::Result<String> {
    let canonical = workspace
        .canonicalize()
        .context("cannot resolve permission workspace")?;
    ensure!(
        canonical.is_dir(),
        "permission workspace must be a directory"
    );
    let value = canonical
        .to_str()
        .context("permission workspace must be valid UTF-8")?;
    validate_target(value)?;
    Ok(value.to_string())
}

fn verified_for(payload: Payload, workspace: &str) -> anyhow::Result<VerifiedGrants> {
    Ok(VerifiedGrants {
        id: payload.content_id()?,
        grants: payload
            .workspaces
            .get(workspace)
            .cloned()
            .unwrap_or_default(),
    })
}

fn read_payload(
    path: &Path,
    root: &UserPublic,
    encryption: &TokenIdentity,
) -> anyhow::Result<Payload> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Payload::empty()),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    // An existing dangling link or unreadable store must not become an empty
    // policy which merge could overwrite. Validate the opened object, and do
    // not block if a special file replaces the path before open.
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "permission store must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take((MAX_STORE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    decode(&bytes, root, encryption)
}

fn encode(payload: &Payload, root: &UserKey, encryption: &TokenIdentity) -> anyhow::Result<String> {
    payload.validate()?;
    let canonical = payload.canonical_form()?;
    ensure!(
        canonical.len() <= MAX_STORE_BYTES,
        "permission store exceeds byte limit"
    );
    let signed = SignedPayload {
        payload: payload.clone(),
        signature: SerdeSig(root.sign(&canonical)),
    };
    let plaintext = signed.canonical_form()?;
    ensure!(
        plaintext.len() <= MAX_STORE_BYTES,
        "permission store exceeds byte limit"
    );
    let ciphertext =
        crate::secrets::encrypt_to_identity(encryption, &plaintext).map_err(anyhow::Error::msg)?;
    ensure!(
        ciphertext.len() <= MAX_STORE_BYTES,
        "encrypted permission store exceeds byte limit"
    );
    Ok(ciphertext)
}

fn decode(bytes: &[u8], root: &UserPublic, encryption: &TokenIdentity) -> anyhow::Result<Payload> {
    ensure!(
        bytes.len() <= MAX_STORE_BYTES,
        "encrypted permission store exceeds byte limit"
    );
    let plaintext = crate::secrets::decrypt_with_identity(encryption, bytes)
        .context("cannot decrypt durable permission store")?;
    ensure!(
        plaintext.len() <= MAX_STORE_BYTES,
        "permission store exceeds byte limit"
    );
    let signed = SignedPayload::from_canonical_form(&plaintext)?;
    root.verify(&signed.payload.canonical_form()?, &signed.signature.0)
        .context("durable permissions do not verify under the trusted operator key")?;
    signed.payload.validate()?;
    Ok(signed.payload)
}

#[cfg(test)]
#[path = "durable_grants_tests.rs"]
mod tests;
