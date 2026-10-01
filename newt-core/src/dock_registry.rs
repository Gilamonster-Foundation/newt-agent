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

/// The authority a dock is approved for — **enforced per operation**, not just
/// signed. `Mirror` may read (list sessions + fetch transcript); `MirrorInject`
/// may additionally enqueue a prompt (D2 — the peer still runs it, staying the
/// sole writer). An unknown token fails to deserialize, so a tampered or
/// forward-dated scope drops the whole record at load (fail-closed) rather than
/// silently widening to write authority.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DockScope {
    /// List sessions and read transcripts; NO inject.
    Mirror,
    /// Mirror + inject a prompt (D2).
    MirrorInject,
}

impl DockScope {
    /// Both scopes permit reads (list + transcript).
    #[must_use]
    pub fn allows_read(self) -> bool {
        matches!(self, Self::Mirror | Self::MirrorInject)
    }
    /// Only `MirrorInject` permits enqueuing a prompt.
    #[must_use]
    pub fn allows_inject(self) -> bool {
        matches!(self, Self::MirrorInject)
    }
    /// The canonical token used in the signed preimage and on disk.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Mirror => "mirror",
            Self::MirrorInject => "mirror-inject",
        }
    }
    /// Parse an operator-supplied scope (CLI). Least authority is the safe
    /// default the caller should pick when unsure.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s.trim() {
            "mirror" => Ok(Self::Mirror),
            "mirror-inject" => Ok(Self::MirrorInject),
            other => {
                anyhow::bail!("unknown dock scope `{other}` (use `mirror` or `mirror-inject`)")
            }
        }
    }
}

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
    /// The typed authority this dock is approved for (enforced per operation).
    pub scope: DockScope,
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
    scope: DockScope,
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

/// Revoke a dock using the operator identity at `identity_pem` — the path-only
/// twin of [`revoke_dock`] (see [`load_docks_with_identity`] for the seam).
pub fn revoke_dock_with_identity(
    config_path: &Path,
    identity_pem: &Path,
    peer_fingerprint_prefix: &str,
) -> anyhow::Result<String> {
    let root = UserKey::load(identity_pem)?;
    revoke_dock(config_path, peer_fingerprint_prefix, &root)
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
    scope: DockScope,
    transcript_id: &str,
    root_key: &UserKey,
) -> anyhow::Result<PathBuf> {
    if peer_agent_fingerprint.is_empty() {
        anyhow::bail!("a peer agent fingerprint is required");
    }
    // Reject a decoupled approval at the source: the fingerprint the dock is
    // keyed under must be the pubkey's own BLAKE3. Without this, a mis-wired
    // caller could sign fingerprint(B)+pubkey(C); verify_record would later drop
    // it, but bailing here makes the inconsistent record unrepresentable rather
    // than silently unusable.
    if !fingerprint_binds_pubkey(peer_agent_fingerprint, peer_pubkey) {
        anyhow::bail!(
            "peer fingerprint {peer_agent_fingerprint} is not BLAKE3(pubkey) — refusing to sign a decoupled approval"
        );
    }
    let issuer = root_key.public().fingerprint().hex();
    let dir = docks_dir(config_path);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{SUBJECT}.toml"));

    // Hold the lock across the WHOLE read-modify-write so two concurrent
    // `newt dock approve` cannot lost-update each other; the atomic write keeps
    // a crash from truncating peers.toml (losing every approval).
    let _lock = crate::atomic_fs::acquire_lock(&crate::atomic_fs::lock_path_for(&path))?;
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
        scope,
        issued_generation: next_generation,
        transcript_id: transcript_id.to_owned(),
        revoked: false,
        sig: None,
    };
    bundle
        .docks
        .push(sign_record(&issuer, SUBJECT, record, root_key));
    crate::atomic_fs::atomic_write(&path, toml::to_string(&bundle)?.as_bytes())?;
    Ok(path)
}

/// Revoke an approved dock by peer-fingerprint prefix, returning the full
/// fingerprint.
///
/// Revocation is a *signed* edit, not a deletion: the row stays, its `revoked`
/// flag flips and its generation bumps, then the whole row is re-signed. A
/// tamperer who clears the flag by hand invalidates the signature, so the row
/// is dropped at load rather than coming back to life — which is why this needs
/// the root key exactly as [`approve_dock`] does.
///
/// Linearization: the dock responder re-reads this registry on **every** request
/// (`NewtDockService` → `authorize_caller` → `load_docks_with_identity` →
/// `approved`), so once a revocation is committed to `peers.toml`, the very next
/// request from that caller resolves to `None` and is denied — including an
/// in-flight sequence of requests. `approved()` already excludes `revoked` rows,
/// so no additional generation check is needed for the request/reply transport.
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
    let _lock = crate::atomic_fs::acquire_lock(&crate::atomic_fs::lock_path_for(&path))?;
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
    crate::atomic_fs::atomic_write(&path, toml::to_string(&bundle)?.as_bytes())?;
    Ok(fingerprint)
}

/// Revoke *every* live dock atomically — the `/undock all` kill-switch. Returns
/// the fingerprints revoked.
pub fn revoke_all_docks(config_path: &Path, root_key: &UserKey) -> anyhow::Result<Vec<String>> {
    let issuer = root_key.public().fingerprint().hex();
    let path = docks_dir(config_path).join(format!("{SUBJECT}.toml"));
    let _lock = crate::atomic_fs::acquire_lock(&crate::atomic_fs::lock_path_for(&path))?;
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
        crate::atomic_fs::atomic_write(&path, toml::to_string(&bundle)?.as_bytes())?;
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

/// Decode a mesh agent public key from exactly 64 lowercase/uppercase hex chars
/// into its 32 bytes. The one decoder the registry, the CLI, and the web gate
/// share so "an agent pubkey" is validated the same way everywhere (no third
/// open-coded loop). Returns `None` on any malformed input — wrong length, or a
/// non-hex nibble — so callers fail closed.
#[must_use]
pub fn decode_agent_pubkey(hex: &str) -> Option<[u8; 32]> {
    decode_hex::<32>(hex)
}

/// Decode exactly `N` bytes from `2 * N` hex chars; `None` on anything else.
fn decode_hex<const N: usize>(hex: &str) -> Option<[u8; N]> {
    let hex = hex.trim();
    if hex.len() != 2 * N {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Whether `fingerprint` is genuinely `BLAKE3(pubkey)` for a canonically-decoded
/// pubkey. This is the binding the docs always claimed but never enforced: a
/// signed record must not be able to pair fingerprint(B) with pubkey(C). Fails
/// closed on malformed pubkey hex.
#[must_use]
fn fingerprint_binds_pubkey(fingerprint: &str, pubkey_hex: &str) -> bool {
    match decode_agent_pubkey(pubkey_hex) {
        Some(bytes) => agent_fingerprint_of_pubkey(&bytes) == fingerprint,
        None => false,
    }
}

/// The RP-id slot for a dock ceremony transcript — the analogue of a passkey
/// relying party, naming the ceremony so a dock transcript can never collide
/// with an enrollment one.
const DOCK_RP_ID: &str = "newt-dock/v1";
/// Agent keys are Ed25519, so the transcript's COSE-alg slot carries `-8`
/// (`EdDSA`). It is a domain constant, not a negotiated value.
const DOCK_COSE_ALG: i64 = -8;

/// A deterministic six-word mnemonic of a peer agent pubkey.
///
/// This is NOT a two-party SAS: it is a friendly rendering of the (public) key
/// itself, so **any** party that knows the pubkey derives the identical words
/// with no exchanged secret and no round-trip. That is exactly what makes it a
/// real cross-check for same-operator docking — the peer's own newt-web prints
/// these same words when it binds its dock service, and the approving operator
/// confirms they match. (A genuine anti-MITM two-party SAS, where each side
/// contributes fresh entropy, is the cross-operator Phase-6 work; there is no
/// online adversary to grind against when both ends already hold the same key.)
#[must_use]
pub fn pubkey_words(pubkey: &[u8; 32]) -> [&'static str; crate::sas_transcript::SAS_WORD_COUNT] {
    let mut payload = Vec::with_capacity(64);
    payload.extend_from_slice(b"newt/dock-pubkey-words/v1");
    push_field(&mut payload, pubkey);
    crate::sas_transcript::sas_words(&Fingerprint::of_bytes(&payload))
}

/// Digits in a [`Pairing::code`].
pub const PAIRING_CODE_DIGITS: usize = 6;

/// The commitment a host sends first when it pairs with a hub (Numeric
/// Comparison, K8.8): it binds both dock keys to the host's secret nonce, so
/// the host is committed to that nonce before it sees the hub's.
#[must_use]
pub fn pairing_commitment(
    hub_pubkey: &[u8; 32],
    host_pubkey: &[u8; 32],
    host_nonce: &[u8; 16],
) -> [u8; 32] {
    let mut payload = Vec::with_capacity(112);
    payload.extend_from_slice(b"newt/dock-pairing-commit/v1");
    push_field(&mut payload, hub_pubkey);
    push_field(&mut payload, host_pubkey);
    push_field(&mut payload, host_nonce);
    Fingerprint::of_bytes(&payload).0
}

/// A completed pairing of a hub and a host (Numeric Comparison, K8.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pairing {
    /// The [`PAIRING_CODE_DIGITS`]-digit code both operators compare before
    /// either side approves the other.
    pub code: String,
    /// What was paired: both keys, the commitment and both nonces.
    pub transcript: PairingTranscript,
    /// The transcript's content id — what each side's signed approval
    /// commits to as its `transcript_id`.
    pub transcript_id: String,
}

/// The record of one Numeric Comparison pairing, content-addressed so the id
/// both signed approvals carry is the dag-cbor CID of what was compared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingTranscript {
    /// The pairing protocol, `newt/dock-pairing/v1`.
    pub protocol: String,
    /// The hub's dock key, hex.
    pub hub_pubkey: String,
    /// The host's dock key, hex.
    pub host_pubkey: String,
    /// The host's commitment to its nonce, hex.
    pub host_commitment: String,
    /// The hub's nonce, hex.
    pub hub_nonce: String,
    /// The host's nonce, hex, which opens its commitment.
    pub host_nonce: String,
}

impl content_addressable::ContentAddressable for PairingTranscript {
    fn canonical_form(&self) -> Result<Vec<u8>, content_addressable::ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}

/// Derive the pairing of a hub and host from both dock keys, in role order,
/// the host's commitment and both nonces. Neither end can steer the code: the
/// host committed to its nonce before seeing the hub's, and the hub chose its
/// nonce before seeing the host's. `None` only if the transcript cannot be
/// encoded.
#[must_use]
pub fn pairing(
    hub_pubkey: &[u8; 32],
    host_pubkey: &[u8; 32],
    hub_nonce: &[u8; 16],
    host_nonce: &[u8; 16],
) -> Option<Pairing> {
    use content_addressable::ContentAddressable as _;
    let transcript = PairingTranscript {
        protocol: "newt/dock-pairing/v1".into(),
        hub_pubkey: hex(hub_pubkey),
        host_pubkey: hex(host_pubkey),
        host_commitment: hex(&pairing_commitment(hub_pubkey, host_pubkey, host_nonce)),
        hub_nonce: hex(hub_nonce),
        host_nonce: hex(host_nonce),
    };
    let id = transcript.content_id().ok()?;
    // The code is its own derivation from the transcript, domain-separated
    // from the id, so displaying it reveals nothing the id is used for.
    let mut payload = b"newt/dock-pairing-code/v1".to_vec();
    push_field(&mut payload, &transcript.canonical_form().ok()?);
    let digest = Fingerprint::of_bytes(&payload);
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest.0[..8]);
    Some(Pairing {
        code: format!("{:06}", u64::from_be_bytes(head) % 1_000_000),
        transcript,
        transcript_id: id.to_string(),
    })
}

/// The human-verifiable output of a dock approval: the 6-word mnemonic of the
/// peer pubkey (reproducible by the peer — see [`pubkey_words`]) plus the
/// transcript id the signed [`DockRecord`] commits to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockCeremony {
    /// The transcript id, hex — stored as `DockRecord::transcript_id`. Binds the
    /// approval to the exact pubkey + a fresh nonce (per-approval uniqueness).
    pub transcript_id: String,
    /// The peer pubkey's 6-word mnemonic ([`pubkey_words`]). The operator
    /// confirms these match the words the peer's own newt-web prints — a
    /// fingerprint cross-check, not a two-party SAS.
    pub sas_words: [&'static str; crate::sas_transcript::SAS_WORD_COUNT],
    /// The commitment to `(peer_pubkey, blinding)`, hex — carried so a real
    /// two-party ceremony (Phase 6, cross-operator) can open it; unused in the
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
    use crate::sas_transcript::{commit, TranscriptInputs};
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
        // Display words are the PUBKEY's mnemonic (reproducible by the peer), not
        // the secret transcript's — so "compare with the peer's display" is a
        // check the peer can actually satisfy.
        sas_words: pubkey_words(peer_pubkey),
        commitment: commitment.hex(),
    }
}

/// How long an unapproved host stays staged after its last uplink poll.
pub const STAGED_HOST_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Most hosts staged at once. Staging a new host past this is refused, so
/// distinct proposals cannot grow the store without bound.
pub const MAX_STAGED_HOSTS: usize = 64;

/// A host that uplinked to this hub without an approval (K8.5): a proposal,
/// never an authority. It is unsigned, and nothing reads it but
/// `newt dock approve --staged`, which turns it into a signed [`DockRecord`]
/// only after the operator confirms its key words at this terminal (the K4
/// stage-then-promote rule). It expires [`STAGED_HOST_TTL`] after the host's
/// last poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedHost {
    /// `BLAKE3(peer_pubkey)`, as the hub verified it against the poll's signer.
    pub peer_agent_fingerprint: String,
    /// The host's dock agent public key, hex.
    pub peer_pubkey: String,
    /// The host's dock instance name, as the host reported it. Unverified: the
    /// operator confirms the key, not the name.
    pub peer_label: String,
    /// When the host last polled, in seconds since the Unix epoch.
    pub last_seen_unix: u64,
    /// The dock key of the hub that staged it, hex.
    pub hub_pubkey: String,
    /// The host's pairing commitment, hex, once it has sent one (K8.8).
    #[serde(default)]
    pub host_commitment: Option<String>,
    /// The hub's pairing nonce, hex, sent in reply to that commitment.
    #[serde(default)]
    pub hub_nonce: Option<String>,
    /// The host's pairing nonce, hex, once revealed and checked against its
    /// commitment.
    #[serde(default)]
    pub host_nonce: Option<String>,
}

impl StagedHost {
    /// The completed pairing, once the host has revealed a nonce that opens
    /// its commitment. `None` while pairing is incomplete or does not verify.
    #[must_use]
    pub fn pairing(&self) -> Option<Pairing> {
        let hub = decode_hex::<32>(&self.hub_pubkey)?;
        let host = decode_hex::<32>(&self.peer_pubkey)?;
        let commitment = decode_hex::<32>(self.host_commitment.as_deref()?)?;
        let hub_nonce = decode_hex::<16>(self.hub_nonce.as_deref()?)?;
        let host_nonce = decode_hex::<16>(self.host_nonce.as_deref()?)?;
        (pairing_commitment(&hub, &host, &host_nonce) == commitment)
            .then(|| pairing(&hub, &host, &hub_nonce, &host_nonce))
            .flatten()
    }
}

/// A pairing step a host's poll carries to its hub (K8.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingStep {
    /// Start (or restart) pairing: the host's commitment.
    Commit([u8; 32]),
    /// The host's nonce, opening its commitment.
    Reveal([u8; 16]),
}

/// The hub's answer to a [`PairingStep`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingReply {
    /// The hub's fresh nonce, answering a commitment.
    Nonce([u8; 16]),
    /// The nonce opened the commitment: pairing is complete.
    Paired,
    /// The nonce did not open the commitment, or there was none: pairing is
    /// cleared and must restart.
    Failed,
}

impl content_addressable::ContentAddressable for StagedHost {
    fn canonical_form(&self) -> Result<Vec<u8>, content_addressable::ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}

/// A staged host as stored: the proposal and its content id, so a reader can
/// recompute the id and detect an edited proposal. The file is named for the
/// host's fingerprint, which only locates it.
#[derive(Debug, Serialize, Deserialize)]
struct StagedFile {
    id: content_addressable::ContentId,
    host: StagedHost,
}

/// Stage (or refresh) an unapproved host that polled this hub. Only the
/// fingerprint of `pubkey` names the file, so a host cannot write another's.
///
/// # Errors
/// A label that is not a plain instance name (1–80 of `[A-Za-z0-9._-]`), or
/// the staging file could not be written.
pub fn stage_host(
    config_path: &Path,
    pubkey: &[u8; 32],
    label: &str,
    hub_pubkey: &[u8; 32],
    now: std::time::SystemTime,
) -> anyhow::Result<()> {
    if !is_host_label(label) {
        anyhow::bail!("host label {label:?} is not a plain instance name");
    }
    let peer_agent_fingerprint = agent_fingerprint_of_pubkey(pubkey);
    let staged = StagedHost {
        peer_pubkey: hex(pubkey),
        peer_label: label.to_owned(),
        last_seen_unix: unix_secs(now),
        hub_pubkey: hex(hub_pubkey),
        peer_agent_fingerprint,
        host_commitment: None,
        hub_nonce: None,
        host_nonce: None,
    };
    let dir = staged_dir(config_path);
    std::fs::create_dir_all(&dir)?;
    let _lock = crate::atomic_fs::acquire_lock(&crate::atomic_fs::lock_path_for(&dir))?;
    let live = read_staged(&dir, now, true);
    let staged_count = live.len();
    let mut staged = staged;
    match live
        .into_iter()
        .find(|held| held.peer_agent_fingerprint == staged.peer_agent_fingerprint)
    {
        // A refresh keeps the pairing under way with this same hub.
        Some(held) if held.hub_pubkey == staged.hub_pubkey => {
            staged.host_commitment = held.host_commitment;
            staged.hub_nonce = held.hub_nonce;
            staged.host_nonce = held.host_nonce;
        }
        Some(_) => {}
        None if staged_count >= MAX_STAGED_HOSTS => {
            anyhow::bail!("{MAX_STAGED_HOSTS} hosts are already staged; approve or wait them out");
        }
        None => {}
    }
    write_staged(&dir, staged)
}

/// Apply a host's pairing `step` to its staged record, and answer it (K8.8).
/// A commitment (re)starts pairing with `fresh_nonce` as the hub's nonce; a
/// reveal completes it only if it opens the stored commitment, and otherwise
/// clears it. The host must already be staged by this hub.
///
/// # Errors
/// The host is not staged (or not by this hub), or the record cannot be
/// written.
pub fn pair_staged_host(
    config_path: &Path,
    host_pubkey: &[u8; 32],
    hub_pubkey: &[u8; 32],
    step: PairingStep,
    fresh_nonce: [u8; 16],
    now: std::time::SystemTime,
) -> anyhow::Result<PairingReply> {
    let dir = staged_dir(config_path);
    let _lock = crate::atomic_fs::acquire_lock(&crate::atomic_fs::lock_path_for(&dir))?;
    let fp = agent_fingerprint_of_pubkey(host_pubkey);
    let hub_hex = hex(hub_pubkey);
    let mut staged = read_staged(&dir, now, false)
        .into_iter()
        .find(|held| held.peer_agent_fingerprint == fp && held.hub_pubkey == hub_hex)
        .ok_or_else(|| anyhow::anyhow!("host {fp} is not staged by this hub"))?;
    let reply = match step {
        PairingStep::Commit(commitment) => {
            staged.host_commitment = Some(hex(&commitment));
            staged.hub_nonce = Some(hex(&fresh_nonce));
            staged.host_nonce = None;
            PairingReply::Nonce(fresh_nonce)
        }
        PairingStep::Reveal(host_nonce) => {
            let opens = staged
                .host_commitment
                .as_deref()
                .and_then(decode_hex::<32>)
                .filter(|_| staged.hub_nonce.is_some())
                .is_some_and(|c| pairing_commitment(hub_pubkey, host_pubkey, &host_nonce) == c);
            if opens {
                staged.host_nonce = Some(hex(&host_nonce));
                PairingReply::Paired
            } else {
                staged.host_commitment = None;
                staged.hub_nonce = None;
                staged.host_nonce = None;
                PairingReply::Failed
            }
        }
    };
    write_staged(&dir, staged)?;
    Ok(reply)
}

fn write_staged(dir: &Path, staged: StagedHost) -> anyhow::Result<()> {
    use content_addressable::ContentAddressable as _;
    let file = StagedFile {
        id: staged.content_id()?,
        host: staged,
    };
    let path = dir.join(format!("{}.toml", file.host.peer_agent_fingerprint));
    crate::atomic_fs::atomic_write(&path, toml::to_string(&file)?.as_bytes())?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The hosts staged and live at `now`, by label: observed no later than `now`
/// and less than [`STAGED_HOST_TTL`] before it. A file that does not parse,
/// whose content id does not match its proposal, that is not named for its own
/// key's fingerprint, or whose label [`stage_host`] would refuse is skipped.
#[must_use]
pub fn staged_hosts(config_path: &Path, now: std::time::SystemTime) -> Vec<StagedHost> {
    read_staged(&staged_dir(config_path), now, false)
}

/// Read the live staged hosts in `dir`; with `reap`, delete every other file
/// there (expired, from the future, or failing verification).
fn read_staged(dir: &Path, now: std::time::SystemTime, reap: bool) -> Vec<StagedHost> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let now = unix_secs(now);
    let oldest = now.saturating_sub(STAGED_HOST_TTL.as_secs());
    let mut hosts: Vec<StagedHost> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension()? != "toml" {
                return None;
            }
            let host = verified_staged(&path)
                .filter(|host| host.last_seen_unix > oldest && host.last_seen_unix <= now);
            if host.is_none() && reap {
                let _ = std::fs::remove_file(&path);
            }
            host
        })
        .collect();
    hosts.sort_by(|a, b| a.peer_label.cmp(&b.peer_label));
    hosts
}

fn verified_staged(path: &Path) -> Option<StagedHost> {
    use content_addressable::ContentAddressable as _;
    let file: StagedFile = toml::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let host = file.host;
    let addressed = host.content_id().ok()? == file.id;
    let named_for_key = path.file_stem()?.to_str()? == host.peer_agent_fingerprint;
    (addressed
        && named_for_key
        && fingerprint_binds_pubkey(&host.peer_agent_fingerprint, &host.peer_pubkey)
        && decode_agent_pubkey(&host.hub_pubkey).is_some()
        && is_host_label(&host.peer_label))
    .then_some(host)
}

/// Forget a staged host, once it is approved. Already gone is not an error.
///
/// # Errors
/// The staging file exists but could not be removed.
pub fn unstage_host(config_path: &Path, peer_agent_fingerprint: &str) -> anyhow::Result<()> {
    let dir = staged_dir(config_path);
    if !dir.exists() {
        return Ok(());
    }
    let _lock = crate::atomic_fs::acquire_lock(&crate::atomic_fs::lock_path_for(&dir))?;
    remove_staged(&dir, peer_agent_fingerprint)
}

/// Promote the staged host `peer_agent_fingerprint` under the pairing its
/// operator compared, `transcript_id`: `approve` signs the approval, and the
/// staged record is consumed. All of it holds the staging lock the pairing
/// steps take, so the host cannot pair again between the check and the
/// signature, and a newer pairing is never consumed in the old one's place.
/// Lock order: staging, then (inside `approve`) the registry.
///
/// # Errors
/// The host is no longer staged or no longer holds that pairing — nothing is
/// signed and the record is left as it is — or `approve` fails, or the staged
/// record cannot be removed.
pub fn promote_staged_host<T>(
    config_path: &Path,
    peer_agent_fingerprint: &str,
    transcript_id: &str,
    approve: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let dir = staged_dir(config_path);
    let _lock = crate::atomic_fs::acquire_lock(&crate::atomic_fs::lock_path_for(&dir))?;
    let held = read_staged(&dir, std::time::SystemTime::now(), false)
        .into_iter()
        .find(|host| host.peer_agent_fingerprint == peer_agent_fingerprint)
        .and_then(|host| host.pairing());
    if held.is_none_or(|p| p.transcript_id != transcript_id) {
        anyhow::bail!("the host no longer holds the pairing compared; nothing was approved");
    }
    let approved = approve()?;
    remove_staged(&dir, peer_agent_fingerprint)?;
    Ok(approved)
}

fn remove_staged(dir: &Path, peer_agent_fingerprint: &str) -> anyhow::Result<()> {
    match std::fs::remove_file(dir.join(format!("{peer_agent_fingerprint}.toml"))) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

fn is_host_label(label: &str) -> bool {
    (1..=80).contains(&label.len())
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn unix_secs(at: std::time::SystemTime) -> u64 {
    at.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn staged_dir(config_path: &Path) -> PathBuf {
    config_path.with_file_name("ocap").join("docks.staged")
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
    // The fingerprint MUST be the pubkey's own BLAKE3 — otherwise a validly
    // signed record could authorize a dial to pubkey(C) while carrying
    // fingerprint(B). Both load_docks and approved() funnel through here, so this
    // one check closes the binding at load AND at dial time, fail-closed.
    if !fingerprint_binds_pubkey(&record.peer_agent_fingerprint, &record.peer_pubkey) {
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
    push_field(&mut payload, record.scope.as_wire().as_bytes());
    push_field(&mut payload, &record.issued_generation.to_be_bytes());
    push_field(&mut payload, record.transcript_id.as_bytes());
    push_field(&mut payload, &[u8::from(record.revoked)]);
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use content_addressable::ContentAddressable as _;
    use tempfile::TempDir;

    /// A pubkey whose every byte is `seed`, as 64 hex chars.
    fn pk_hex(seed: u8) -> String {
        format!("{seed:02x}").repeat(32)
    }
    /// The genuine mesh fingerprint of `pk_hex(seed)` — a real BLAKE3(pubkey)
    /// pair, so the record satisfies the binding the registry now enforces.
    fn fp(seed: u8) -> String {
        agent_fingerprint_of_pubkey(&[seed; 32])
    }

    fn record() -> DockRecord {
        DockRecord {
            peer_agent_fingerprint: fp(0),
            peer_label: "laptop-b".into(),
            peer_pubkey: pk_hex(0),
            scope: DockScope::MirrorInject,
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
        assert!(registry.approved(&fp(0)).is_some());
        assert!(registry.approved("unknown").is_none());
    }

    /// Linux adversarial (symlink handling of `docks.d`): `read_dir` +
    /// `read_to_string` both FOLLOW symlinks, so an attacker who can plant a
    /// symlink in the registry dir could point it at a bundle they control. The
    /// operator-root SIGNATURE — not the path — is the boundary: a symlinked
    /// bundle signed by a FOREIGN key is followed, read, and dropped, so
    /// symlink-following is not an authority bypass. macOS follows symlinks the
    /// same way; this pins that the decision is content-signature-based and
    /// identical on both platforms (no divergence).
    #[test]
    #[cfg(unix)]
    fn a_symlinked_docks_entry_with_a_foreign_signature_is_dropped() {
        use std::os::unix::fs::symlink;
        let root = UserKey::generate(); // the real operator
        let attacker = UserKey::generate(); // a foreign key the attacker holds
                                            // The attacker signs a dock (approving pk 0) with THEIR key, out-of-tree.
        let issuer = attacker.fingerprint().hex();
        let signed = sign_record(&issuer, SUBJECT, record(), &attacker);
        let bundle = DockBundle {
            issuer,
            subject: SUBJECT.into(),
            docks: vec![signed],
        };
        let dir = TempDir::new().unwrap();
        // Write the attacker bundle OUTSIDE the registry dir…
        let external = dir.path().join("attacker-bundle.toml");
        std::fs::write(&external, toml::to_string(&bundle).unwrap()).unwrap();
        // …then plant a symlink to it inside ocap/docks.d/.
        let docks = dir.path().join("ocap/docks.d");
        std::fs::create_dir_all(&docks).unwrap();
        symlink(&external, docks.join("evil.toml")).unwrap();

        // Loaded against the REAL operator root: the symlinked foreign bundle is
        // followed, read, and DROPPED (foreign issuer). No approval crosses.
        let (registry, warnings) =
            load_docks(&dir.path().join("config.toml"), Some(&root.public()));
        assert!(
            registry.approved(&fp(0)).is_none(),
            "a foreign-signed symlinked dock must not approve anyone"
        );
        assert!(registry.is_empty());
        assert!(
            warnings.iter().any(|w| w.contains("foreign issuer")),
            "the drop must be surfaced to the operator: {warnings:?}"
        );
    }

    /// Linux adversarial (registry-dir case-sensitivity): the authority decision
    /// is content-based (exact fingerprint string + operator signature), never
    /// filesystem-case-based. (1) The `.toml` extension match is case-exact, so a
    /// `peers.TOML` bundle is NOT loaded — fail-closed on a case-mismatched name,
    /// never silently admitted. (2) `approved()` compares the fingerprint
    /// exactly, with no case-folding, so an upper-cased variant of an approved
    /// fingerprint is denied. On a case-INSENSITIVE fs (macOS default) filenames
    /// may collide, but the signature/fingerprint decision is identical — the
    /// only divergence is which files `read_dir` surfaces, never who is approved.
    #[test]
    fn the_authority_decision_is_case_exact_not_filesystem_case_dependent() {
        let root = UserKey::generate();
        let issuer = root.fingerprint().hex();

        // A correctly-signed bundle, but saved with an UPPER-CASE extension.
        let dir = TempDir::new().unwrap();
        let docks = dir.path().join("ocap/docks.d");
        std::fs::create_dir_all(&docks).unwrap();
        std::fs::write(
            docks.join("peers.TOML"),
            toml::to_string(&DockBundle {
                issuer: issuer.clone(),
                subject: SUBJECT.into(),
                docks: vec![sign_record(&issuer, SUBJECT, record(), &root)],
            })
            .unwrap(),
        )
        .unwrap();
        let (registry, _w) = load_docks(&dir.path().join("config.toml"), Some(&root.public()));
        assert!(
            registry.is_empty(),
            "a .TOML-extension bundle must not be loaded (extension match is case-exact, fail-closed)"
        );

        // A properly-named bundle: the exact fingerprint is approved, an
        // upper-cased fingerprint (a different string) is not.
        let dir2 = TempDir::new().unwrap();
        write_bundle(
            &dir2,
            &DockBundle {
                issuer: issuer.clone(),
                subject: SUBJECT.into(),
                docks: vec![sign_record(&issuer, SUBJECT, record(), &root)],
            },
        );
        let (registry2, _w2) = load_docks(&dir2.path().join("config.toml"), Some(&root.public()));
        assert!(
            registry2.approved(&fp(0)).is_some(),
            "the exact fingerprint is approved"
        );
        assert!(
            registry2.approved(&fp(0).to_uppercase()).is_none(),
            "an upper-cased fingerprint is NOT approved (no case-folding)"
        );
    }

    #[test]
    fn a_tampered_scope_or_foreign_issuer_is_dropped() {
        let root = UserKey::generate();
        let other = UserKey::generate();
        let issuer = root.fingerprint().hex();
        // Widen the scope on disk after signing — the signature no longer covers it.
        let mut signed = sign_record(&issuer, SUBJECT, record(), &root);
        signed.scope = DockScope::Mirror;
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
            &fp(0),
            "laptop-b",
            &pk_hex(0),
            DockScope::MirrorInject,
            "tx-1",
            &root,
        )
        .unwrap();
        let (registry, _) = load_docks(&config, Some(&root.public()));
        assert!(registry.approved(&fp(0)).is_some());

        let full = revoke_dock(&config, &fp(0)[..8], &root).unwrap();
        assert_eq!(full, fp(0));
        let (registry, _) = load_docks(&config, Some(&root.public()));
        assert!(
            registry.approved(&fp(0)).is_none(),
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
            &fp(0),
            "laptop-b",
            &pk_hex(0),
            DockScope::MirrorInject,
            "tx-1",
            &root,
        )
        .unwrap();
        let (registry, warnings) = load_docks_with_identity(&config, &identity);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert!(registry.approved(&fp(0)).is_some());

        // A missing key fails closed — nothing is approved.
        let (empty, warnings) = load_docks_with_identity(&config, &dir.path().join("nope.pem"));
        assert!(empty.approved(&fp(0)).is_none());
        assert!(!warnings.is_empty());
    }

    #[test]
    fn re_approval_is_idempotent_and_bumps_generation() {
        let root = UserKey::generate();
        let dir = TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        approve_dock(
            &config,
            &fp(0),
            "laptop-b",
            &pk_hex(0),
            DockScope::MirrorInject,
            "tx-1",
            &root,
        )
        .unwrap();
        approve_dock(
            &config,
            &fp(0),
            "laptop-b-renamed",
            &pk_hex(0),
            DockScope::MirrorInject,
            "tx-2",
            &root,
        )
        .unwrap();
        let (registry, _) = load_docks(&config, Some(&root.public()));
        assert_eq!(registry.len(), 1, "re-approval replaces, never duplicates");
        let dock = registry.approved(&fp(0)).unwrap();
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
            &fp(0),
            "a",
            &pk_hex(0),
            DockScope::MirrorInject,
            "tx-a",
            &root,
        )
        .unwrap();
        approve_dock(
            &config,
            &fp(1),
            "b",
            &pk_hex(1),
            DockScope::MirrorInject,
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
        tampered.docks[0].peer_label.push('!');
        std::fs::write(&path, toml::to_string(&tampered).unwrap()).unwrap();
        assert!(
            registry.approved(&fp(0)).is_none(),
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
    fn dock_display_words_come_from_the_pubkey_so_the_peer_can_reproduce_them() {
        let pubkey = [3u8; 32];
        // The display words are the pubkey's own mnemonic — anyone holding the
        // (public) key derives them identically, which is what lets the peer's
        // newt-web show the SAME words for the operator to compare.
        assert_eq!(pubkey_words(&pubkey), pubkey_words(&pubkey));
        assert_ne!(pubkey_words(&pubkey), pubkey_words(&[4u8; 32]));

        let a = dock_ceremony("issuer-fp", "laptop-b", &pubkey, b"nonce-1", b"blind-1");
        assert_eq!(a.sas_words, pubkey_words(&pubkey));

        // A fresh nonce gives a fresh transcript_id (per-approval record
        // uniqueness) but the SAME display words — the words track the KEY, not a
        // secret the peer never sees.
        let rerun = dock_ceremony("issuer-fp", "laptop-b", &pubkey, b"nonce-2", b"blind-1");
        assert_eq!(a.sas_words, rerun.sas_words);
        assert_ne!(a.transcript_id, rerun.transcript_id);

        // A different peer pubkey → different words: this is what catches a
        // swapped key.
        let other = dock_ceremony("issuer-fp", "laptop-b", &[4u8; 32], b"nonce-1", b"blind-1");
        assert_ne!(a.sas_words, other.sas_words);
    }

    #[test]
    fn a_record_binding_one_fingerprint_to_another_pubkey_is_refused_and_dropped() {
        let root = UserKey::generate();
        let dir = TempDir::new().unwrap();
        let config = dir.path().join("config.toml");

        // The write path refuses to SIGN a decoupled approval: fingerprint(B)
        // with pubkey(C).
        let err = approve_dock(
            &config,
            &fp(1), // fingerprint of key B
            "impostor",
            &pk_hex(2), // pubkey of key C
            DockScope::MirrorInject,
            "tx-x",
            &root,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not BLAKE3(pubkey)"));

        // And even a validly-SIGNED decoupled record is dropped at load: an
        // attacker who can sign cannot smuggle fp(B)+pubkey(C) past verify_record.
        let issuer = root.fingerprint().hex();
        let decoupled = DockRecord {
            peer_agent_fingerprint: fp(1),
            peer_pubkey: pk_hex(2),
            ..record()
        };
        let dir2 = TempDir::new().unwrap();
        write_bundle(
            &dir2,
            &DockBundle {
                issuer: issuer.clone(),
                subject: SUBJECT.into(),
                docks: vec![sign_record(&issuer, SUBJECT, decoupled, &root)],
            },
        );
        let (registry, warnings) =
            load_docks(&dir2.path().join("config.toml"), Some(&root.public()));
        assert!(
            registry.is_empty(),
            "a decoupled fp/pubkey record must not load"
        );
        assert!(!warnings.is_empty());
        assert!(registry.approved(&fp(1)).is_none());
    }

    #[test]
    fn a_malformed_pubkey_record_is_dropped() {
        let root = UserKey::generate();
        let issuer = root.fingerprint().hex();
        for bad in ["zz".repeat(32), "00".repeat(20), String::new()] {
            let mut rec = record();
            rec.peer_pubkey = bad;
            // Keep the fingerprint as-is; the pubkey no longer decodes, so the
            // binding check fails closed regardless.
            let dir = TempDir::new().unwrap();
            write_bundle(
                &dir,
                &DockBundle {
                    issuer: issuer.clone(),
                    subject: SUBJECT.into(),
                    docks: vec![sign_record(&issuer, SUBJECT, rec, &root)],
                },
            );
            let (registry, _) = load_docks(&dir.path().join("config.toml"), Some(&root.public()));
            assert!(
                registry.is_empty(),
                "a malformed-pubkey record must not load"
            );
        }
    }

    /// Six concurrent approvals must all survive — the LOST-UPDATE property.
    ///
    /// Without the write lock, N concurrent `newt dock approve` processes each
    /// read the same bundle and the last writer clobbers the rest. The lock
    /// serializes the read-modify-write, so every distinct approval survives.
    ///
    /// **The workers RETRY on contention, and that is the fix for #1871.** This
    /// test used to `unwrap()` in every worker, which asserted something it is
    /// not named for: that all six writers win the lock inside
    /// `acquire_lock`'s ambient budget (100 polls x 20ms). A writer that is
    /// refused and told "try again" has NOT lost an update — it has been
    /// correctly serialized, and it wrote nothing, because `approve_dock`
    /// takes the lock with `?` before it reads. The property under test is the
    /// FINAL CONTENTS, so the workers honour the contract the error states and
    /// the assertion stays on the merged result.
    ///
    /// Verified by mutation rather than by argument: with the budget cut to 2
    /// polls the old shape fails with the exact CI signature while this shape
    /// passes with all six records — the failure was acquisition, not loss.
    /// And with the `acquire_lock` line deleted from `approve_dock`, this
    /// shape fails on the count, so retrying did not make it vacuous.
    #[test]
    fn concurrent_approvals_do_not_lost_update() {
        let root = UserKey::generate();
        let dir = TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        let identity = dir.path().join("identity.pem");
        root.save(&identity).unwrap();

        let handles: Vec<_> = (0u8..6)
            .map(|s| {
                let config = config.clone();
                let identity = identity.clone();
                std::thread::spawn(move || {
                    // Generous, and it bounds a hang rather than a race: the
                    // loop only continues while the registry is actively
                    // telling us someone else holds the lock.
                    let deadline = std::time::Instant::now() + crate::test_guard::HANG_GUARD;
                    loop {
                        match approve_dock_with_identity(
                            &config,
                            &identity,
                            &agent_fingerprint_of_pubkey(&[s; 32]),
                            &format!("peer-{s}"),
                            &format!("{s:02x}").repeat(32),
                            DockScope::MirrorInject,
                            "tx",
                        ) {
                            Ok(_) => return,
                            Err(error) if crate::atomic_fs::is_lock_contended(&error) => {
                                assert!(
                                    std::time::Instant::now() < deadline,
                                    "peer-{s} never acquired the lock within the hang guard — \
                                     that is a stuck lock, not contention: {error:#}"
                                );
                                std::thread::yield_now();
                            }
                            // Anything else is a real failure and must not be
                            // retried into a timeout that hides it.
                            Err(error) => {
                                panic!("peer-{s} failed for a non-contention reason: {error:#}")
                            }
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let (registry, _) = load_docks(&config, Some(&root.public()));
        assert_eq!(
            registry.live().len(),
            6,
            "every concurrent approval must survive the lock-serialized write"
        );
    }

    #[test]
    fn dock_scope_permits_per_operation_and_parses_least_authority() {
        assert!(DockScope::Mirror.allows_read());
        assert!(!DockScope::Mirror.allows_inject());
        assert!(DockScope::MirrorInject.allows_read());
        assert!(DockScope::MirrorInject.allows_inject());
        assert_eq!(DockScope::parse("mirror").unwrap(), DockScope::Mirror);
        assert_eq!(
            DockScope::parse("mirror-inject").unwrap(),
            DockScope::MirrorInject
        );
        assert!(DockScope::parse("co-drive").is_err());
        assert!(DockScope::parse("").is_err());
    }

    #[test]
    fn an_unknown_scope_token_drops_the_whole_record_fail_closed() {
        let root = UserKey::generate();
        let issuer = root.fingerprint().hex();
        // Sign a valid record, then rewrite the on-disk scope to an unknown
        // token. It must fail to DESERIALIZE, dropping the bundle at load — a
        // forward/tampered scope can never widen to inject authority.
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
        let path = dir.path().join("ocap/docks.d/peers.toml");
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("mirror-inject", "co-drive");
        std::fs::write(&path, text).unwrap();
        let (registry, warnings) =
            load_docks(&dir.path().join("config.toml"), Some(&root.public()));
        assert!(registry.is_empty(), "an unknown scope must not load");
        assert!(!warnings.is_empty());
    }

    #[test]
    fn decode_agent_pubkey_round_trips_and_rejects_bad_input() {
        assert_eq!(decode_agent_pubkey(&pk_hex(7)), Some([7u8; 32]));
        assert!(decode_agent_pubkey("short").is_none());
        assert!(decode_agent_pubkey(&"zz".repeat(32)).is_none());
        assert!(fingerprint_binds_pubkey(&fp(3), &pk_hex(3)));
        assert!(!fingerprint_binds_pubkey(&fp(3), &pk_hex(4)));
    }

    /// AUDIENCE MODEL (`docs/decisions/newt_web_docking.md`, "Dock grant audience"):
    /// dock grants are **location-scoped bearer authorization records**. The signed
    /// preimage does not name the resource-owning responder, so a valid grant
    /// authorizing caller `A`, copied verbatim from responder `B`'s `docks.d` into
    /// responder `C`'s `docks.d` (distinct agents under one operator `UserKey`),
    /// resolves at `C` — **but only when `C` also holds the operator's PRIVATE root
    /// key beside the registry.** That co-location is the whole theorem: the write
    /// boundary of `docks.d` is the same filesystem boundary that holds the root
    /// key, so anyone who can place a record there could equally have minted it, and
    /// a machine that merely copied `docks.d` without the root key gets nothing. No
    /// mesh request writes `docks.d`, so this is never a remote replay — it is an
    /// operator-authority action. This pins both halves.
    #[test]
    fn a_dock_grant_is_a_location_scoped_bearer_record_gated_by_root_key_possession() {
        let operator = UserKey::generate();

        // Responder B: the operator approves caller A (pk 0) to dock into B.
        let dir_b = TempDir::new().unwrap();
        let config_b = dir_b.path().join("config.toml");
        let identity_b = dir_b.path().join("identity.pem");
        operator.save(&identity_b).unwrap();
        approve_dock(
            &config_b,
            &fp(0),
            "caller-a",
            &pk_hex(0),
            DockScope::MirrorInject,
            "tx",
            &operator,
        )
        .unwrap();
        let (reg_b, _) = load_docks_with_identity(&config_b, &identity_b);
        assert!(
            reg_b.approved(&fp(0)).is_some(),
            "A must resolve at its own responder B"
        );

        // Copy B's SIGNED peers.toml verbatim into responder C's docks.d.
        let dir_c = TempDir::new().unwrap();
        let config_c = dir_c.path().join("config.toml");
        let docks_c = docks_dir(&config_c);
        std::fs::create_dir_all(&docks_c).unwrap();
        let bytes = std::fs::read(docks_dir(&config_b).join(format!("{SUBJECT}.toml"))).unwrap();
        std::fs::write(docks_c.join(format!("{SUBJECT}.toml")), &bytes).unwrap();

        // (1) C WITHOUT the co-located operator root key: the copied grant is
        // INERT. `load_docks_with_identity` fail-closes when it cannot load the
        // private key, so a synced registry alone authorizes no one.
        let (reg_c_no_key, warnings) =
            load_docks_with_identity(&config_c, &dir_c.path().join("identity.pem"));
        assert!(
            reg_c_no_key.approved(&fp(0)).is_none(),
            "a copied grant must NOT resolve without the co-located operator root key"
        );
        assert!(
            !warnings.is_empty(),
            "the missing root key must surface as an operator-visible warning"
        );

        // (2) C WITH the operator root key co-located (same operator): the grant
        // resolves — location-scoped bearer semantics. Placing the record here
        // required operator-level write to C's protected state dir, the SAME
        // boundary that holds the root key; the operator could equally have minted
        // A->C directly. This is an operator-authority action, not a remote replay.
        let identity_c = dir_c.path().join("identity.pem");
        operator.save(&identity_c).unwrap();
        let (reg_c_with_key, _) = load_docks_with_identity(&config_c, &identity_c);
        assert!(
            reg_c_with_key.approved(&fp(0)).is_some(),
            "with the co-located operator root key the bearer grant resolves"
        );
    }

    const HUB: [u8; 32] = [9; 32];

    #[test]
    fn a_pairing_code_is_six_digits_bound_to_both_keys_and_both_nonces() {
        let p = pairing(&[1; 32], &[2; 32], &[3; 16], &[4; 16]).unwrap();
        assert_eq!(p.code.len(), PAIRING_CODE_DIGITS);
        let id = p.transcript.content_id().unwrap();
        assert_eq!(
            p.transcript_id,
            id.to_string(),
            "the id is the transcript's CID"
        );
        assert_eq!(
            p.transcript_id
                .parse::<content_addressable::ContentId>()
                .unwrap(),
            id
        );
        assert!(p.code.bytes().all(|b| b.is_ascii_digit()), "{}", p.code);
        assert_eq!(
            p,
            pairing(&[1; 32], &[2; 32], &[3; 16], &[4; 16]).unwrap(),
            "deterministic"
        );
        for other in [
            pairing(&[2; 32], &[1; 32], &[3; 16], &[4; 16]),
            pairing(&[1; 32], &[2; 32], &[5; 16], &[4; 16]),
            pairing(&[1; 32], &[2; 32], &[3; 16], &[5; 16]),
        ] {
            assert_ne!(other.unwrap().transcript_id, p.transcript_id);
        }
    }

    /// The hub side of Numeric Comparison: a commitment gets a fresh hub
    /// nonce; only the nonce that opens it completes pairing, to the code the
    /// host derives itself; a wrong nonce clears it; a refresh keeps it.
    #[test]
    fn a_staged_host_pairs_only_by_opening_its_commitment() {
        let dir = TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        let (host, host_nonce, hub_nonce) = ([7u8; 32], [5u8; 16], [6u8; 16]);
        let commit = PairingStep::Commit(pairing_commitment(&HUB, &host, &host_nonce));
        let step = |s| pair_staged_host(&config, &host, &HUB, s, hub_nonce, at(1)).unwrap();
        let live = || staged_hosts(&config, at(1)).remove(0);

        assert!(
            pair_staged_host(&config, &host, &HUB, commit, hub_nonce, at(1)).is_err(),
            "only a staged host pairs"
        );
        stage_host(&config, &host, "nuc1", &HUB, at(1)).unwrap();
        assert_eq!(step(commit), PairingReply::Nonce(hub_nonce));
        assert_eq!(live().pairing(), None, "not complete before the reveal");

        assert_eq!(step(PairingStep::Reveal([0; 16])), PairingReply::Failed);
        assert_eq!(
            live().host_commitment,
            None,
            "a failed reveal clears pairing"
        );

        step(commit);
        assert_eq!(step(PairingStep::Reveal(host_nonce)), PairingReply::Paired);
        let expected = pairing(&HUB, &host, &hub_nonce, &host_nonce).unwrap();
        assert_eq!(live().pairing(), Some(expected.clone()));

        stage_host(&config, &host, "nuc1", &HUB, at(1)).unwrap();
        assert_eq!(
            live().pairing(),
            Some(expected),
            "a refresh keeps the pairing"
        );
        stage_host(&config, &host, "nuc1", &[8; 32], at(1)).unwrap();
        assert_eq!(live().pairing(), None, "another hub's staging starts over");
    }

    /// A staged host is promoted only under the pairing its operator compared,
    /// and atomically: a pairing step that races the promotion waits for it,
    /// then finds the record consumed rather than re-pairing a host whose old
    /// pairing is being signed (#2616).
    #[test]
    fn a_staged_host_is_promoted_only_under_the_pairing_compared_and_atomically() {
        let dir = TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        let (host, now) = ([7u8; 32], std::time::SystemTime::now());
        let fp = agent_fingerprint_of_pubkey(&host);
        let step = |s| pair_staged_host(&config, &host, &HUB, s, [6; 16], now);
        stage_host(&config, &host, "nuc1", &HUB, now).unwrap();
        step(PairingStep::Commit(pairing_commitment(
            &HUB, &host, &[5; 16],
        )))
        .unwrap();
        step(PairingStep::Reveal([5; 16])).unwrap();
        let compared = staged_hosts(&config, now)[0].pairing().unwrap();

        let refused = promote_staged_host(&config, &fp, "another", || -> anyhow::Result<()> {
            panic!("nothing is signed under a pairing the host does not hold")
        });
        assert!(refused.is_err());
        assert_eq!(staged_hosts(&config, now).len(), 1, "the record is left");

        let (started, racing) = std::sync::mpsc::channel();
        let promoted = promote_staged_host(&config, &fp, &compared.transcript_id, || {
            let config = config.clone();
            let race = std::thread::spawn(move || {
                started.send(()).unwrap();
                let commit = PairingStep::Commit(pairing_commitment(&HUB, &host, &[8; 16]));
                pair_staged_host(&config, &host, &HUB, commit, [6; 16], now)
            });
            racing.recv().unwrap();
            // Long enough for an unlocked step to land before the signature.
            std::thread::sleep(std::time::Duration::from_millis(200));
            Ok(race)
        })
        .unwrap();
        assert!(
            promoted.join().unwrap().is_err(),
            "the racing step waited, then found the host promoted"
        );
        assert!(staged_hosts(&config, now).is_empty(), "consumed");
    }

    fn at(secs: u64) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)
    }

    #[test]
    fn a_staged_host_is_listed_until_it_expires_and_gone_once_unstaged() {
        let dir = TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        let pubkey = [7u8; 32];
        stage_host(&config, &pubkey, "nuc1", &HUB, at(1_000)).unwrap();

        let staged = staged_hosts(&config, at(1_000));
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].peer_label, "nuc1");
        assert_eq!(
            staged[0].peer_agent_fingerprint,
            agent_fingerprint_of_pubkey(&pubkey)
        );
        assert_eq!(decode_agent_pubkey(&staged[0].peer_pubkey), Some(pubkey));

        let ttl = STAGED_HOST_TTL.as_secs();
        assert_eq!(staged_hosts(&config, at(1_000 + ttl - 1)).len(), 1);
        assert!(staged_hosts(&config, at(1_000 + ttl)).is_empty(), "expired");

        stage_host(&config, &pubkey, "nuc1", &HUB, at(1_000 + ttl)).unwrap();
        assert_eq!(staged_hosts(&config, at(1_000 + ttl)).len(), 1, "refreshed");

        unstage_host(&config, &staged[0].peer_agent_fingerprint).unwrap();
        assert!(staged_hosts(&config, at(1_000 + ttl)).is_empty());
        unstage_host(&config, &staged[0].peer_agent_fingerprint).unwrap();
    }

    #[test]
    fn a_label_that_is_not_an_instance_name_is_not_staged() {
        let dir = TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        for label in ["", "nuc 1", "nuc\n1", "../x", &"a".repeat(81)] {
            assert!(
                stage_host(&config, &[7u8; 32], label, &HUB, at(1)).is_err(),
                "{label:?}"
            );
        }
        assert!(staged_hosts(&config, at(1)).is_empty());
    }

    #[test]
    fn a_staged_file_not_named_for_its_own_key_is_skipped() {
        let dir = TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        stage_host(&config, &[7u8; 32], "nuc1", &HUB, at(1)).unwrap();
        let staged_dir = staged_dir(&config);
        let genuine = staged_dir.join(format!("{}.toml", agent_fingerprint_of_pubkey(&[7u8; 32])));
        let other = agent_fingerprint_of_pubkey(&[8u8; 32]);
        std::fs::rename(genuine, staged_dir.join(format!("{other}.toml"))).unwrap();
        assert!(staged_hosts(&config, at(1)).is_empty());
    }

    #[test]
    fn an_edited_proposal_no_longer_matches_its_content_id() {
        let dir = TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        stage_host(&config, &[7u8; 32], "nuc1", &HUB, at(1)).unwrap();
        let path =
            staged_dir(&config).join(format!("{}.toml", agent_fingerprint_of_pubkey(&[7u8; 32])));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"nuc1\""), "{text}");
        std::fs::write(&path, text.replace("\"nuc1\"", "\"nuc9\"")).unwrap();
        assert!(
            staged_hosts(&config, at(1)).is_empty(),
            "an edited label is not trusted"
        );
    }

    #[test]
    fn a_proposal_observed_in_the_future_is_not_live() {
        let dir = TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        stage_host(&config, &[7u8; 32], "nuc1", &HUB, at(1_000)).unwrap();
        assert!(staged_hosts(&config, at(999)).is_empty());
        assert_eq!(staged_hosts(&config, at(1_000)).len(), 1);
    }

    #[test]
    fn staging_reaps_dead_entries_and_caps_the_store() {
        let dir = TempDir::new().unwrap();
        let config = dir.path().join("config.toml");
        let key = |i: usize| {
            let mut k = [0u8; 32];
            k[..8].copy_from_slice(&(i as u64).to_be_bytes());
            k
        };
        for i in 0..MAX_STAGED_HOSTS {
            stage_host(&config, &key(i), &format!("h{i}"), &HUB, at(1)).unwrap();
        }
        assert!(
            stage_host(&config, &key(MAX_STAGED_HOSTS), "late", &HUB, at(1)).is_err(),
            "a new host past the cap is refused"
        );
        stage_host(&config, &key(0), "h0", &HUB, at(1)).unwrap();

        let files = || std::fs::read_dir(staged_dir(&config)).unwrap().count();
        assert_eq!(files(), MAX_STAGED_HOSTS);
        let later = at(1 + STAGED_HOST_TTL.as_secs());
        stage_host(&config, &key(MAX_STAGED_HOSTS), "late", &HUB, later).unwrap();
        assert_eq!(files(), 1, "expired entries are reaped when staging");
    }
}
