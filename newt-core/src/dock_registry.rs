//! Signed, fail-closed approved-dock registry.
//!
//! A dock is a standing relationship: the operator, at a hub cockpit, approves
//! another of their own agents to surface its sessions into that hub over
//! agent-mesh. The mesh handshake already proves the peer holds the *same*
//! operator `UserKey` (same-operator docking, decision K2) — so this registry
//! is not authenticating a stranger. It records the operator's **intent** to
//! dock a specific peer agent, scoped and revocable, so the hub can refuse a
//! peer the operator never approved and drop one the operator later revokes.
//!
//! It mirrors [`crate::credential_registry`] exactly — a domain-tagged,
//! length-framed preimage signed by the operator root `UserKey`, dropped at
//! load if it does not verify, and re-read + re-verified from disk at resolve
//! time so a row edited after startup is never trusted. The peer's identity is
//! its mesh **agent fingerprint** (`BLAKE3(agent_pubkey)`); the pubkey is stored
//! too so the hub can rebuild the dial endpoint and confirm the fingerprint is
//! the pubkey's, not a decoupled label.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agent_mesh_protocol::{Fingerprint, SerdeSig, UserKey, UserPublic};
use serde::{Deserialize, Serialize};

use crate::wire_framing::push_field;

const DOMAIN: &[u8] = b"newt/dock-registry/v1";

/// The single bundle subject. Docks are not per-subject the way passkeys are
/// (one operator, many approved peers), so they share one file.
const SUBJECT: &str = "peers";

/// One approved dock, signed by the operator root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DockRecord {
    /// The approved peer's mesh agent fingerprint (`BLAKE3(agent_pubkey)`, hex).
    /// This is the handle the hub resolves before it will dial the peer.
    pub peer_agent_fingerprint: String,
    /// The operator-facing label for the peer (e.g. `laptop-b`). Signed so a
    /// tamperer cannot relabel an approval onto a different machine's UI.
    pub peer_label: String,
    /// The peer's agent public key (hex of the 32-byte verifying key). The hub
    /// rebuilds the dial endpoint from this and checks
    /// `fingerprint == BLAKE3(pubkey)` — a label can never point the approval at
    /// a key it was not signed for.
    pub peer_pubkey: String,
    /// The scope the dock is approved for (coarse token in the same-operator
    /// phase, e.g. `mirror-inject`). Signed so it cannot be widened on disk.
    pub scope: String,
    /// Monotonic approval generation. Revocation bumps it via re-sign.
    pub issued_generation: u64,
    /// The SAS ceremony transcript this approval was minted under — the
    /// public-key + shared-secret evidence binding the approval to the exact
    /// peer pubkey and a fresh nonce (requirement 5).
    pub transcript_id: String,
    pub revoked: bool,
    pub sig: Option<SerdeSig>,
}

/// One `(issuer, subject)` registry file. Subject is outside each row but is in
/// the signature preimage, exactly as the credential registry does it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DockBundle {
    pub issuer: String,
    pub subject: String,
    pub docks: Vec<DockRecord>,
}

/// The in-memory registry. It retains each source path so every resolve can
/// re-read and reconstruct the signed preimage rather than trusting a cached
/// row.
#[derive(Debug, Clone, Default)]
pub struct DockRegistry {
    bundles: BTreeMap<(String, String), DockBundle>,
    sources: BTreeMap<(String, String), PathBuf>,
    root: Option<UserPublic>,
}

/// Load `~/.newt/ocap/docks.d/*.toml`, dropping every unverifiable row.
///
/// Missing roots, foreign issuers, malformed files, unsigned rows, and bad
/// signatures all fail closed and are returned as operator-visible warnings.
pub fn load_docks(
    config_path: &Path,
    root_key: Option<&UserPublic>,
) -> (DockRegistry, Vec<String>) {
    let mut registry = DockRegistry {
        bundles: BTreeMap::new(),
        sources: BTreeMap::new(),
        root: root_key.cloned(),
    };
    let mut warnings = Vec::new();
    let dir = docks_dir(config_path);
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
        let bundle: DockBundle = match toml::from_str(&text) {
            Ok(bundle) => bundle,
            Err(error) => {
                warnings.push(format!("{}: malformed docks ({error})", path.display()));
                continue;
            }
        };
        let Some(root) = root_key else {
            if !bundle.docks.is_empty() {
                warnings.push(format!(
                    "{}: no operator root; docks dropped",
                    path.display()
                ));
            }
            continue;
        };
        if !issuer_matches(&bundle.issuer, root) {
            warnings.push(format!("{}: foreign issuer; docks dropped", path.display()));
            continue;
        }
        let mut kept = Vec::new();
        for record in bundle.docks {
            if verify_record(&bundle.issuer, &bundle.subject, &record, root).is_ok() {
                kept.push(record);
            } else {
                warnings.push(format!(
                    "{}: unsigned or tampered dock dropped",
                    path.display()
                ));
            }
        }
        if !kept.is_empty() {
            let key = (bundle.issuer.clone(), bundle.subject.clone());
            registry.bundles.insert(
                key.clone(),
                DockBundle {
                    docks: kept,
                    ..bundle
                },
            );
            registry.sources.insert(key, path);
        }
    }
    (registry, warnings)
}

/// Load the registry against the operator identity at `identity_pem`, for
/// callers that hold only the on-disk key path and must not name a specific
/// `agent-mesh-protocol` build (newt-web links a *path* copy for the mesh dial
/// client while this crate links the registry copy — passing a `UserPublic`
/// across that seam would not type-check). Fail-closed: if the key cannot be
/// loaded, nothing is approved.
pub fn load_docks_with_identity(
    config_path: &Path,
    identity_pem: &Path,
) -> (DockRegistry, Vec<String>) {
    match UserKey::load(identity_pem) {
        Ok(key) => load_docks(config_path, Some(&key.public())),
        Err(error) => (
            DockRegistry::default(),
            vec![format!("{}: {error}", identity_pem.display())],
        ),
    }
}

/// Approve a dock using the operator identity at `identity_pem`, for callers
/// that hold only the key path (see [`load_docks_with_identity`] for why the
/// `UserKey` type cannot cross the newt-web seam). Mainly a test seam so a
/// path-side crate can seed an approval without a registry-typed key.
pub fn approve_dock_with_identity(
    config_path: &Path,
    identity_pem: &Path,
    peer_agent_fingerprint: &str,
    peer_label: &str,
    peer_pubkey: &str,
    scope: &str,
    transcript_id: &str,
) -> anyhow::Result<PathBuf> {
    let root = UserKey::load(identity_pem)?;
    approve_dock(
        config_path,
        peer_agent_fingerprint,
        peer_label,
        peer_pubkey,
        scope,
        transcript_id,
        &root,
    )
}

impl DockRegistry {
    /// Resolve an approved, non-revoked dock by peer agent fingerprint,
    /// re-reading and re-verifying against the retained operator root. This is
    /// the dial-time read boundary: a row edited after startup is not trusted.
    #[must_use]
    pub fn approved(&self, peer_agent_fingerprint: &str) -> Option<DockRecord> {
        let root = self.root.as_ref()?;
        for ((issuer, subject), source) in &self.sources {
            let Ok(text) = std::fs::read_to_string(source) else {
                continue;
            };
            let Ok(bundle) = toml::from_str::<DockBundle>(&text) else {
                continue;
            };
            if &bundle.issuer != issuer || &bundle.subject != subject {
                continue;
            }
            if let Some(record) = bundle.docks.iter().find(|record| {
                !record.revoked
                    && record.peer_agent_fingerprint == peer_agent_fingerprint
                    && verify_record(&bundle.issuer, &bundle.subject, record, root).is_ok()
            }) {
                return Some(record.clone());
            }
        }
        None
    }

    /// Every live (non-revoked) approved dock, for `newt dock list` and the
    /// hub's dock overview. Re-verified from memory (already fail-closed at
    /// load); dial still re-reads via [`Self::approved`].
    #[must_use]
    pub fn live(&self) -> Vec<DockRecord> {
        self.bundles
            .values()
            .flat_map(|bundle| bundle.docks.iter())
            .filter(|record| !record.revoked)
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.bundles.values().map(|bundle| bundle.docks.len()).sum()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Sign a dock record with the operator root. Pure; binds every security field.
#[must_use]
pub fn sign_record(
    issuer: &str,
    subject: &str,
    mut record: DockRecord,
    root_key: &UserKey,
) -> DockRecord {
    record.sig = Some(SerdeSig(
        root_key.sign(&signing_payload(issuer, subject, &record)),
    ));
    record
}

/// Approve a dock by appending a signed record to `docks.d/peers.toml`.
///
/// The registry's only write path, terminal-gated by construction: the caller
/// must hold the operator root key, the issuer is taken from the key (never the
/// caller), and a row this function did not sign does not survive the next
/// [`load_docks`]. Re-approving the same peer replaces the old row rather than
/// stacking duplicates, so approval is idempotent and a re-ceremony refreshes
/// the transcript/scope in place.
pub fn approve_dock(
    config_path: &Path,
    peer_agent_fingerprint: &str,
    peer_label: &str,
    peer_pubkey: &str,
    scope: &str,
    transcript_id: &str,
    root_key: &UserKey,
) -> anyhow::Result<PathBuf> {
    if peer_agent_fingerprint.is_empty() {
        anyhow::bail!("a peer agent fingerprint is required");
    }
    let issuer = root_key.public().fingerprint().hex();
    let dir = docks_dir(config_path);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{SUBJECT}.toml"));

    let mut bundle = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<DockBundle>(&text)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DockBundle {
            issuer: issuer.clone(),
            subject: SUBJECT.to_owned(),
            docks: Vec::new(),
        },
        Err(error) => return Err(error.into()),
    };
    if bundle.issuer != issuer || bundle.subject != SUBJECT {
        anyhow::bail!("{} belongs to another operator", path.display());
    }

    // Carry the highest generation seen for this peer so a re-approval never
    // regresses a revocation's generation bump.
    let next_generation = bundle
        .docks
        .iter()
        .filter(|held| held.peer_agent_fingerprint == peer_agent_fingerprint)
        .map(|held| held.issued_generation + 1)
        .max()
        .unwrap_or(0);
    bundle
        .docks
        .retain(|held| held.peer_agent_fingerprint != peer_agent_fingerprint);

    let record = DockRecord {
        peer_agent_fingerprint: peer_agent_fingerprint.to_owned(),
        peer_label: peer_label.to_owned(),
        peer_pubkey: peer_pubkey.to_owned(),
        scope: scope.to_owned(),
        issued_generation: next_generation,
        transcript_id: transcript_id.to_owned(),
        revoked: false,
        sig: None,
    };
    bundle
        .docks
        .push(sign_record(&issuer, SUBJECT, record, root_key));
    std::fs::write(&path, toml::to_string(&bundle)?)?;
    Ok(path)
}

/// Revoke an approved dock by peer-fingerprint prefix, returning the full
/// fingerprint.
///
/// Revocation is a *signed* edit, not a deletion: the row stays, its `revoked`
/// flag flips and its generation bumps, then the whole row is re-signed. A
/// tamperer who clears the flag by hand invalidates the signature, so the row
/// is dropped at load rather than coming back to life — which is why this needs
/// the root key exactly as [`approve_dock`] does. The generation bump is what a
/// live responder's `verify_at(gen)` re-check trips on, closing an in-flight
/// dock rather than only refusing the next one.
pub fn revoke_dock(
    config_path: &Path,
    peer_fingerprint_prefix: &str,
    root_key: &UserKey,
) -> anyhow::Result<String> {
    if peer_fingerprint_prefix.is_empty() {
        anyhow::bail!("a peer fingerprint prefix is required");
    }
    let issuer = root_key.public().fingerprint().hex();
    let path = docks_dir(config_path).join(format!("{SUBJECT}.toml"));
    let text = std::fs::read_to_string(&path)
        .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
    let mut bundle: DockBundle = toml::from_str(&text)?;
    if bundle.issuer != issuer || bundle.subject != SUBJECT {
        anyhow::bail!("{} belongs to another operator", path.display());
    }

    let matches: Vec<usize> = bundle
        .docks
        .iter()
        .enumerate()
        .filter(|(_, held)| {
            held.peer_agent_fingerprint
                .starts_with(peer_fingerprint_prefix)
                && !held.revoked
        })
        .map(|(index, _)| index)
        .collect();
    let [index] = matches[..] else {
        anyhow::bail!(
            "`{peer_fingerprint_prefix}` matches {} live docks; use a longer prefix",
            matches.len()
        );
    };

    let mut record = bundle.docks[index].clone();
    record.revoked = true;
    record.issued_generation += 1;
    let fingerprint = record.peer_agent_fingerprint.clone();
    bundle.docks[index] = sign_record(&issuer, SUBJECT, record, root_key);
    std::fs::write(&path, toml::to_string(&bundle)?)?;
    Ok(fingerprint)
}

/// Revoke *every* live dock atomically — the `/undock all` kill-switch. Returns
/// the fingerprints revoked.
pub fn revoke_all_docks(config_path: &Path, root_key: &UserKey) -> anyhow::Result<Vec<String>> {
    let issuer = root_key.public().fingerprint().hex();
    let path = docks_dir(config_path).join(format!("{SUBJECT}.toml"));
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut bundle: DockBundle = toml::from_str(&text)?;
    if bundle.issuer != issuer || bundle.subject != SUBJECT {
        anyhow::bail!("{} belongs to another operator", path.display());
    }
    let mut revoked = Vec::new();
    for record in &mut bundle.docks {
        if !record.revoked {
            record.revoked = true;
            record.issued_generation += 1;
            revoked.push(record.peer_agent_fingerprint.clone());
            *record = sign_record(&issuer, SUBJECT, record.clone(), root_key);
        }
    }
    if !revoked.is_empty() {
        std::fs::write(&path, toml::to_string(&bundle)?)?;
    }
    Ok(revoked)
}

/// The mesh agent fingerprint for a raw agent public key — the handle a peer is
/// approved under. Kept here so the hub's dial path and the ceremony compute it
/// the one same way.
#[must_use]
pub fn agent_fingerprint_of_pubkey(pubkey: &[u8; 32]) -> String {
    Fingerprint::of_bytes(pubkey).hex()
}

/// The RP-id slot for a dock ceremony transcript — the analogue of a passkey
/// relying party, naming the ceremony so a dock transcript can never collide
/// with an enrollment one.
const DOCK_RP_ID: &str = "newt-dock/v1";
/// Agent keys are Ed25519, so the transcript's COSE-alg slot carries `-8`
/// (`EdDSA`). It is a domain constant, not a negotiated value.
const DOCK_COSE_ALG: i64 = -8;

/// The human-verifiable output of a dock ceremony: the 6-word SAS the operator
/// reads to confirm *which* peer they are approving, plus the transcript id the
/// signed [`DockRecord`] commits to. Reuses the passkey SAS machinery
/// ([`crate::sas_transcript`]) verbatim so there is one transcript codec, not
/// two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockCeremony {
    /// The transcript id, hex — stored as `DockRecord::transcript_id`.
    pub transcript_id: String,
    /// Six BIP-39 words derived from the transcript. The operator compares
    /// these against the peer's own display to confirm the exact pubkey.
    pub sas_words: [&'static str; crate::sas_transcript::SAS_WORD_COUNT],
    /// The commitment to `(peer_pubkey, blinding)`, hex — carried so a two-sided
    /// ceremony (Phase 6, cross-operator) can open it; unused in the
    /// single-terminal same-operator flow beyond binding the transcript.
    pub commitment: String,
}

/// Derive the dock ceremony transcript + SAS words for approving `peer_pubkey`
/// under `issuer`, folding a fresh `nonce` and `blinding` into the passkey
/// transcript so the words are unpredictable per ceremony and bound to the
/// exact peer key. Pure — the caller supplies the randomness so the derivation
/// is testable with fixed vectors.
#[must_use]
pub fn dock_ceremony(
    issuer: &str,
    peer_label: &str,
    peer_pubkey: &[u8; 32],
    nonce: &[u8],
    blinding: &[u8],
) -> DockCeremony {
    use crate::sas_transcript::{commit, sas_words, TranscriptInputs};
    let commitment = commit(peer_pubkey, blinding);
    let peer_fp = Fingerprint::of_bytes(peer_pubkey).hex();
    let inputs = TranscriptInputs {
        rp_id: DOCK_RP_ID,
        issuer,
        subject: peer_label,
        mesh_agent_fingerprint: &peer_fp,
        cose_alg: DOCK_COSE_ALG,
        cose_pubkey: peer_pubkey,
        commitment: &commitment,
        enroll_nonce: nonce,
    };
    let transcript = inputs.transcript_id();
    DockCeremony {
        transcript_id: transcript.hex(),
        sas_words: sas_words(&transcript),
        commitment: commitment.hex(),
    }
}

fn docks_dir(config_path: &Path) -> PathBuf {
    config_path.with_file_name("ocap").join("docks.d")
}

fn issuer_matches(issuer: &str, root: &UserPublic) -> bool {
    issuer == root.fingerprint().short() || issuer == root.fingerprint().hex()
}

fn verify_record(
    issuer: &str,
    subject: &str,
    record: &DockRecord,
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

fn signing_payload(issuer: &str, subject: &str, record: &DockRecord) -> Vec<u8> {
    let mut payload = Vec::with_capacity(256);
    payload.extend_from_slice(DOMAIN);
    push_field(&mut payload, issuer.as_bytes());
    push_field(&mut payload, subject.as_bytes());
    push_field(&mut payload, record.peer_agent_fingerprint.as_bytes());
    push_field(&mut payload, record.peer_label.as_bytes());
    push_field(&mut payload, record.peer_pubkey.as_bytes());
    push_field(&mut payload, record.scope.as_bytes());
    push_field(&mut payload, &record.issued_generation.to_be_bytes());
    push_field(&mut payload, record.transcript_id.as_bytes());
    push_field(&mut payload, &[u8::from(record.revoked)]);
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn record() -> DockRecord {
        DockRecord {
            peer_agent_fingerprint: "aabbccdd".into(),
            peer_label: "laptop-b".into(),
            peer_pubkey: "00".repeat(32),
            scope: "mirror-inject".into(),
            issued_generation: 0,
            transcript_id: "tx-1".into(),
            revoked: false,
            sig: None,
        }
    }

    fn write_bundle(dir: &TempDir, bundle: &DockBundle) {
        let path = dir.path().join("ocap/docks.d");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("peers.toml"), toml::to_string(bundle).unwrap()).unwrap();
    }

    #[test]
    fn signed_dock_loads_and_resolves() {
        let root = UserKey::generate();
        let issuer = root.fingerprint().hex();
        let signed = sign_record(&issuer, SUBJECT, record(), &root);
        let dir = TempDir::new().unwrap();
        write_bundle(
            &dir,
            &DockBundle {
                issuer: issuer.clone(),
                subject: SUBJECT.into(),
                docks: vec![signed],
            },
        );
        let (registry, warnings) =
            load_docks(&dir.path().join("config.toml"), Some(&root.public()));
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(registry.len(), 1);
        assert!(registry.approved("aabbccdd").is_some());
        assert!(registry.approved("unknown").is_none());
    }

    #[test]
    fn a_tampered_scope_or_foreign_issuer_is_dropped() {
        let root = UserKey::generate();
        let other = UserKey::generate();
        let issuer = root.fingerprint().hex();
        // Widen the scope on disk after signing — the signature no longer covers it.
        let mut signed = sign_record(&issuer, SUBJECT, record(), &root);
        signed.scope = "co-drive".into();
        let dir = TempDir::new().unwrap();
        write_bundle(
            &dir,
            &DockBundle {
                issuer: other.fingerprint().hex(),
                subject: SUBJECT.into(),
                docks: vec![signed, record()],
            },
        );
        let (registry, warnings) =
            load_docks(&dir.path().join("config.toml"), Some(&root.public()));
        assert!(registry.is_empty());
        assert!(!warnings.is_empty());
    }

    #[test]
    fn approve_then_revoke_removes_the_dock_from_resolve() {
        let root = UserKey::generate();
        let dir = TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        approve_dock(
            &config,
            "aabbccdd",
            "laptop-b",
            &"00".repeat(32),
            "mirror-inject",
            "tx-1",
            &root,
        )
        .unwrap();
        let (registry, _) = load_docks(&config, Some(&root.public()));
        assert!(registry.approved("aabbccdd").is_some());

        let full = revoke_dock(&config, "aabb", &root).unwrap();
        assert_eq!(full, "aabbccdd");
        let (registry, _) = load_docks(&config, Some(&root.public()));
        assert!(
            registry.approved("aabbccdd").is_none(),
            "a revoked dock must never resolve"
        );
        // A revoked row is not `live`, so the overview and the kill-switch agree.
        assert!(registry.live().is_empty());
    }

    #[test]
    fn load_with_identity_resolves_without_crossing_the_userpublic_seam() {
        let root = UserKey::generate();
        let dir = TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        let identity = dir.path().join("identity.pem");
        root.save(&identity).unwrap();
        approve_dock(
            &config,
            "aabbccdd",
            "laptop-b",
            &"00".repeat(32),
            "mirror-inject",
            "tx-1",
            &root,
        )
        .unwrap();
        let (registry, warnings) = load_docks_with_identity(&config, &identity);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert!(registry.approved("aabbccdd").is_some());

        // A missing key fails closed — nothing is approved.
        let (empty, warnings) = load_docks_with_identity(&config, &dir.path().join("nope.pem"));
        assert!(empty.approved("aabbccdd").is_none());
        assert!(!warnings.is_empty());
    }

    #[test]
    fn re_approval_is_idempotent_and_bumps_generation() {
        let root = UserKey::generate();
        let dir = TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        approve_dock(
            &config,
            "aabbccdd",
            "laptop-b",
            &"00".repeat(32),
            "mirror-inject",
            "tx-1",
            &root,
        )
        .unwrap();
        approve_dock(
            &config,
            "aabbccdd",
            "laptop-b-renamed",
            &"00".repeat(32),
            "mirror-inject",
            "tx-2",
            &root,
        )
        .unwrap();
        let (registry, _) = load_docks(&config, Some(&root.public()));
        assert_eq!(registry.len(), 1, "re-approval replaces, never duplicates");
        let dock = registry.approved("aabbccdd").unwrap();
        assert_eq!(dock.peer_label, "laptop-b-renamed");
        assert_eq!(dock.issued_generation, 1);
    }

    #[test]
    fn revoke_all_revokes_every_live_dock() {
        let root = UserKey::generate();
        let dir = TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        approve_dock(
            &config,
            "aaaa",
            "a",
            &"00".repeat(32),
            "mirror-inject",
            "tx-a",
            &root,
        )
        .unwrap();
        approve_dock(
            &config,
            "bbbb",
            "b",
            &"11".repeat(32),
            "mirror-inject",
            "tx-b",
            &root,
        )
        .unwrap();
        let revoked = revoke_all_docks(&config, &root).unwrap();
        assert_eq!(revoked.len(), 2);
        let (registry, _) = load_docks(&config, Some(&root.public()));
        assert!(registry.live().is_empty());
    }

    #[test]
    fn dial_time_read_rejects_a_flipped_byte() {
        let root = UserKey::generate();
        let issuer = root.fingerprint().hex();
        let signed = sign_record(&issuer, SUBJECT, record(), &root);
        let dir = TempDir::new().unwrap();
        write_bundle(
            &dir,
            &DockBundle {
                issuer: issuer.clone(),
                subject: SUBJECT.into(),
                docks: vec![signed],
            },
        );
        let (registry, _) = load_docks(&dir.path().join("config.toml"), Some(&root.public()));
        let path = dir.path().join("ocap/docks.d/peers.toml");
        let mut tampered: DockBundle =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        tampered.docks[0].scope.push('!');
        std::fs::write(&path, toml::to_string(&tampered).unwrap()).unwrap();
        assert!(
            registry.approved("aabbccdd").is_none(),
            "a row edited after load must not resolve"
        );
    }

    #[test]
    fn agent_fingerprint_of_pubkey_matches_the_mesh_convention() {
        let pubkey = [7u8; 32];
        assert_eq!(
            agent_fingerprint_of_pubkey(&pubkey),
            Fingerprint::of_bytes(&pubkey).hex()
        );
    }

    #[test]
    fn dock_ceremony_is_deterministic_and_bound_to_the_pubkey() {
        let pubkey = [3u8; 32];
        let a = dock_ceremony("issuer-fp", "laptop-b", &pubkey, b"nonce-1", b"blind-1");
        // Same inputs → same words (both terminals derive independently).
        let again = dock_ceremony("issuer-fp", "laptop-b", &pubkey, b"nonce-1", b"blind-1");
        assert_eq!(a, again);
        assert_eq!(a.sas_words.len(), 6);

        // A different peer pubkey → different words: the SAS is what the operator
        // reads to catch a swapped key.
        let other = dock_ceremony("issuer-fp", "laptop-b", &[4u8; 32], b"nonce-1", b"blind-1");
        assert_ne!(a.sas_words, other.sas_words);
        assert_ne!(a.transcript_id, other.transcript_id);

        // A fresh nonce → fresh words even for the same peer (per-ceremony).
        let rerun = dock_ceremony("issuer-fp", "laptop-b", &pubkey, b"nonce-2", b"blind-1");
        assert_ne!(a.sas_words, rerun.sas_words);
    }
}
