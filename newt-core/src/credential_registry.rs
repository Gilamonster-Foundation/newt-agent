//! Signed, fail-closed WebAuthn credential bindings.
//!
//! Credential files are operator-owned data, not authority by themselves. A
//! row is usable only when its issuer matches the configured operator root and
//! its signature verifies over every security-bearing field. The same check is
//! repeated by [`CredentialRegistry::resolve`] so an answer-time read cannot
//! trust a row that changed after startup.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agent_mesh_protocol::{SerdeSig, UserKey, UserPublic};
use serde::{Deserialize, Serialize};

use crate::wire_framing::push_field;

const DOMAIN: &[u8] = b"newt/credential-registry/v1";

/// A passkey binding signed by the operator root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CredentialRecord {
    pub credential_id_handle: String,
    /// Canonical COSE public-key bytes, encoded by the file format as base64.
    pub cose_pubkey: String,
    pub cose_alg: i64,
    pub mesh_agent_fingerprint: String,
    pub issued_generation: u64,
    pub transcript_id: String,
    pub revoked: bool,
    pub sig: Option<SerdeSig>,
}

/// One `(issuer, subject)` registry entry. The subject is intentionally outside
/// each row but is still included in the signature preimage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CredentialBundle {
    pub issuer: String,
    pub subject: String,
    pub credentials: Vec<CredentialRecord>,
}

/// The in-memory registry. It retains the source bundle so every resolve can
/// reconstruct the exact signed preimage rather than trusting cached fields.
#[derive(Debug, Clone, Default)]
pub struct CredentialRegistry {
    bundles: BTreeMap<(String, String), CredentialBundle>,
    sources: BTreeMap<(String, String), PathBuf>,
    root: Option<UserPublic>,
}

/// Load `~/.newt/ocap/credentials.d/*.toml`, dropping every unverifiable row.
/// Missing roots, foreign issuers, malformed files, unsigned rows, and bad
/// signatures all fail closed and are returned as operator-visible warnings.
pub fn load_credentials(
    config_path: &Path,
    root_key: Option<&UserPublic>,
) -> (CredentialRegistry, Vec<String>) {
    let mut registry = CredentialRegistry {
        bundles: BTreeMap::new(),
        sources: BTreeMap::new(),
        root: root_key.cloned(),
    };
    let mut warnings = Vec::new();
    let dir = credentials_dir(config_path);
    let mut paths = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (registry, warnings),
        Err(error) => {
            warnings.push(format!("{}: {error}", dir.display()));
            return (registry, warnings);
        }
    };
    paths.sort();

    for path in paths {
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                warnings.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        let bundle: CredentialBundle = match toml::from_str(&text) {
            Ok(bundle) => bundle,
            Err(error) => {
                warnings.push(format!(
                    "{}: malformed credentials ({error})",
                    path.display()
                ));
                continue;
            }
        };
        let Some(root) = root_key else {
            if !bundle.credentials.is_empty() {
                warnings.push(format!(
                    "{}: no operator root; credentials dropped",
                    path.display()
                ));
            }
            continue;
        };
        if !issuer_matches(&bundle.issuer, root) {
            warnings.push(format!(
                "{}: foreign issuer; credentials dropped",
                path.display()
            ));
            continue;
        }
        let mut kept = Vec::new();
        for record in bundle.credentials {
            if verify_record(&bundle.issuer, &bundle.subject, &record, root).is_ok() {
                kept.push(record);
            } else {
                warnings.push(format!(
                    "{}: unsigned or tampered credential dropped",
                    path.display()
                ));
            }
        }
        if !kept.is_empty() {
            let key = (bundle.issuer.clone(), bundle.subject.clone());
            registry.bundles.insert(
                key.clone(),
                CredentialBundle {
                    credentials: kept,
                    ..bundle
                },
            );
            registry.sources.insert(key, path);
        }
    }
    (registry, warnings)
}

/// Return a non-revoked credential only after re-verifying it against the
/// retained operator root. This is the answer-time read boundary.
pub fn resolve_credential(
    registry: &CredentialRegistry,
    issuer: &str,
    subject: &str,
    credential_id_handle: &str,
) -> Option<CredentialRecord> {
    registry.resolve(issuer, subject, credential_id_handle)
}

impl CredentialRegistry {
    /// Re-read and verify one credential binding. A revoked row is never
    /// resolved, even if its signature remains valid.
    pub fn resolve(
        &self,
        issuer: &str,
        subject: &str,
        credential_id_handle: &str,
    ) -> Option<CredentialRecord> {
        let root = self.root.as_ref()?;
        let source = self.sources.get(&(issuer.to_owned(), subject.to_owned()))?;
        let text = std::fs::read_to_string(source).ok()?;
        let bundle: CredentialBundle = toml::from_str(&text).ok()?;
        if bundle.issuer != issuer || bundle.subject != subject {
            return None;
        }
        bundle
            .credentials
            .iter()
            .find(|record| {
                !record.revoked
                    && record.credential_id_handle == credential_id_handle
                    && verify_record(&bundle.issuer, &bundle.subject, record, root).is_ok()
            })
            .cloned()
    }

    /// Number of rows retained after fail-closed verification.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bundles
            .values()
            .map(|bundle| bundle.credentials.len())
            .sum()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Construct a signed record. Persistence and terminal authorization belong to
/// the enrollment slice; this pure helper keeps signing deterministic and
/// binds every security field now.
pub fn sign_record(
    issuer: &str,
    subject: &str,
    mut record: CredentialRecord,
    root_key: &UserKey,
) -> CredentialRecord {
    record.sig = Some(SerdeSig(
        root_key.sign(&signing_payload(issuer, subject, &record)),
    ));
    record
}

/// Append a signed binding to `credentials.d/<subject>.toml`, returning the
/// file written.
///
/// This is the registry's only write path and it is terminal-gated by
/// construction: the caller must hold the operator root key, and a row this
/// function did not sign does not survive the next [`load_credentials`]. The
/// issuer is taken from the key rather than the caller, so a promotion cannot
/// name an issuer it cannot sign for.
///
/// Refuses a subject that is not a bare filename, a bundle belonging to another
/// operator, and a handle already present — the last making promotion safe to
/// retry without shadowing an existing credential.
pub fn append_credential(
    config_path: &Path,
    subject: &str,
    record: CredentialRecord,
    root_key: &UserKey,
) -> anyhow::Result<PathBuf> {
    if subject.is_empty()
        || !subject
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        || subject.starts_with('.')
    {
        anyhow::bail!("subject `{subject}` is not a safe bundle filename");
    }
    let issuer = root_key.public().fingerprint().hex();
    let dir = credentials_dir(config_path);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{subject}.toml"));

    let mut bundle = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<CredentialBundle>(&text)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => CredentialBundle {
            issuer: issuer.clone(),
            subject: subject.to_owned(),
            credentials: Vec::new(),
        },
        Err(error) => return Err(error.into()),
    };
    if bundle.issuer != issuer || bundle.subject != subject {
        anyhow::bail!("{} belongs to another operator", path.display());
    }
    if bundle
        .credentials
        .iter()
        .any(|held| held.credential_id_handle == record.credential_id_handle)
    {
        anyhow::bail!(
            "credential `{}` is already enrolled",
            record.credential_id_handle
        );
    }

    bundle
        .credentials
        .push(sign_record(&issuer, subject, record, root_key));
    std::fs::write(&path, toml::to_string(&bundle)?)?;
    Ok(path)
}

fn credentials_dir(config_path: &Path) -> PathBuf {
    config_path.with_file_name("ocap").join("credentials.d")
}

fn issuer_matches(issuer: &str, root: &UserPublic) -> bool {
    issuer == root.fingerprint().short() || issuer == root.fingerprint().hex()
}

fn verify_record(
    issuer: &str,
    subject: &str,
    record: &CredentialRecord,
    root: &UserPublic,
) -> Result<(), ()> {
    if !issuer_matches(issuer, root) {
        return Err(());
    }
    let Some(signature) = record.sig.as_ref() else {
        return Err(());
    };
    root.verify(&signing_payload(issuer, subject, record), &signature.0)
        .map_err(|_| ())
}

fn signing_payload(issuer: &str, subject: &str, record: &CredentialRecord) -> Vec<u8> {
    let mut payload = Vec::with_capacity(256);
    payload.extend_from_slice(DOMAIN);
    push_field(&mut payload, issuer.as_bytes());
    push_field(&mut payload, subject.as_bytes());
    push_field(&mut payload, record.credential_id_handle.as_bytes());
    push_field(&mut payload, record.cose_pubkey.as_bytes());
    push_field(&mut payload, &record.cose_alg.to_be_bytes());
    push_field(&mut payload, record.mesh_agent_fingerprint.as_bytes());
    push_field(&mut payload, &record.issued_generation.to_be_bytes());
    push_field(&mut payload, record.transcript_id.as_bytes());
    push_field(&mut payload, &[u8::from(record.revoked)]);
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn record() -> CredentialRecord {
        CredentialRecord {
            credential_id_handle: "cred-1".into(),
            cose_pubkey: "p256-key".into(),
            cose_alg: -7,
            mesh_agent_fingerprint: "agent-fp".into(),
            issued_generation: 9,
            transcript_id: "tx-1".into(),
            revoked: false,
            sig: None,
        }
    }

    fn write_bundle(dir: &TempDir, bundle: &CredentialBundle) {
        let path = dir.path().join("ocap/credentials.d");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("operator.toml"), toml::to_string(bundle).unwrap()).unwrap();
    }

    #[test]
    fn signed_record_loads_and_resolves() {
        let root = UserKey::generate();
        let issuer = root.fingerprint().hex();
        let signed = sign_record(&issuer, "operator", record(), &root);
        let dir = TempDir::new().unwrap();
        write_bundle(
            &dir,
            &CredentialBundle {
                issuer: issuer.clone(),
                subject: "operator".into(),
                credentials: vec![signed],
            },
        );
        let (registry, warnings) =
            load_credentials(&dir.path().join("config.toml"), Some(&root.public()));
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(registry.len(), 1);
        assert!(resolve_credential(&registry, &issuer, "operator", "cred-1").is_some());
    }

    #[test]
    fn unsigned_field_swapped_and_foreign_rows_are_dropped() {
        let root = UserKey::generate();
        let other = UserKey::generate();
        let issuer = root.fingerprint().hex();
        let mut signed = sign_record(&issuer, "operator", record(), &root);
        signed.cose_pubkey = "tampered".into();
        let dir = TempDir::new().unwrap();
        write_bundle(
            &dir,
            &CredentialBundle {
                issuer: other.fingerprint().hex(),
                subject: "operator".into(),
                credentials: vec![signed, record()],
            },
        );
        let (registry, warnings) =
            load_credentials(&dir.path().join("config.toml"), Some(&root.public()));
        assert!(registry.is_empty());
        assert!(!warnings.is_empty());
    }

    #[test]
    fn revoked_rows_never_resolve() {
        let root = UserKey::generate();
        let issuer = root.fingerprint().hex();
        let mut revoked = record();
        revoked.revoked = true;
        let dir = TempDir::new().unwrap();
        write_bundle(
            &dir,
            &CredentialBundle {
                issuer: issuer.clone(),
                subject: "operator".into(),
                credentials: vec![sign_record(&issuer, "operator", revoked, &root)],
            },
        );
        let (registry, _) = load_credentials(&dir.path().join("config.toml"), Some(&root.public()));
        assert!(resolve_credential(&registry, &issuer, "operator", "cred-1").is_none());
    }

    #[test]
    fn answer_time_read_rejects_a_flipped_byte() {
        let root = UserKey::generate();
        let issuer = root.fingerprint().hex();
        let signed = sign_record(&issuer, "operator", record(), &root);
        let dir = TempDir::new().unwrap();
        write_bundle(
            &dir,
            &CredentialBundle {
                issuer: issuer.clone(),
                subject: "operator".into(),
                credentials: vec![signed],
            },
        );
        let (registry, _) = load_credentials(&dir.path().join("config.toml"), Some(&root.public()));
        let path = dir.path().join("ocap/credentials.d/operator.toml");
        let mut tampered: CredentialBundle =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        tampered.credentials[0].transcript_id.push('x');
        std::fs::write(&path, toml::to_string(&tampered).unwrap()).unwrap();
        assert!(registry.resolve(&issuer, "operator", "cred-1").is_none());
    }
}
