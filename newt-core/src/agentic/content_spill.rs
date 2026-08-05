//! Content-addressed spill identity (#1528 B3 follow-up to #1538).
//!
//! A spill handle is the **BLAKE3 CIDv1 of the canonical dag-cbor** of a
//! versioned, session-scoped [`SpillRecordV1`] — a self-verifying record, not an
//! allocated id. This dissolves the allocator/reservation bug class the #1538
//! review kept fighting: there is no next sequential id for a concurrent writer to
//! steal, identical content converges on one handle, different content cannot
//! alias, duplicate writes are idempotent, and a rejected candidate needs no id
//! rollback (its CID was never published).
//!
//! **Privacy (equality-leak seal).** A global plaintext CID would leak equality:
//! anyone who can guess a secret-bearing payload could compute its CID and confirm
//! a match. The record therefore carries a per-session nonce ([`SpillScope`]), so
//! identical plaintext in two sessions gets DIFFERENT addresses. Cross-session
//! dedup, if ever wanted, is an explicit trusted-local-store policy — not the
//! default.
//!
//! **Authorization stays separate from identity.** A CID proves "these bytes hash
//! here", never "this agent may read them". The read path is mediated by the
//! session-scoped store (membership + the session nonce) — a model cannot paste a
//! foreign CID and retrieve it: it neither belongs to this session's store nor
//! re-derives under this session's nonce.
//!
//! This is the identity CORE. Wiring the tool-offload (`spill:`) and compaction
//! (`compaction:`) paths onto it — deleting the reservation machinery — is the
//! consumer-migration stage.
//!
//! TRANSITIONAL: until that migration wires these in, the surface has no non-test
//! caller, so the module allows `dead_code`. The allow is removed at cutover.
#![allow(dead_code)]

use content_addressable::{canonical, ContentAddressable, ContentError, ContentId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Current spill-record schema tag. Bumping it re-addresses every spill (a schema
/// migration is a new address space, by construction).
pub const SPILL_SCHEMA_V1: &str = "newt.spill/v1";

/// The privacy scope bound into a spill's identity. `Session` injects a random
/// per-session nonce so the same plaintext addresses differently across sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpillScope {
    /// A per-session nonce — 16 random bytes minted once at session start
    /// (deterministic/fixed in tests).
    Session([u8; 16]),
}

/// Where a spilled payload came from — bound into identity so a tool output and an
/// identical-looking compaction span never share an address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpillProvenance {
    /// An oversized tool result offloaded from context (the `spill:` handle).
    ToolOutput { tool_name: Option<String> },
    /// A redacted middle span elided by the compactor (the `compaction:` handle).
    CompactionSpan,
}

/// The versioned, canonical spill record whose CID IS the handle. Serialized via
/// canonical dag-cbor: equal records ⇒ equal bytes ⇒ equal CID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpillRecordV1 {
    /// Schema tag (see [`SPILL_SCHEMA_V1`]).
    pub schema: String,
    pub scope: SpillScope,
    pub provenance: SpillProvenance,
    /// The ALREADY-REDACTED payload (redact-on-store happens before a record is
    /// ever built — the raw secret never reaches this type).
    pub redacted_text: String,
}

impl SpillRecordV1 {
    pub fn new(scope: SpillScope, provenance: SpillProvenance, redacted_text: String) -> Self {
        Self {
            schema: SPILL_SCHEMA_V1.to_string(),
            scope,
            provenance,
            redacted_text,
        }
    }
}

impl ContentAddressable for SpillRecordV1 {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        canonical::to_canonical_dagcbor(self)
    }
}

/// Why a handle string is not an acceptable [`SpillCid`] under Newt's canonical
/// input policy (STRICTER than the crate's profile check — see [`SpillCid::parse`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpillCidError {
    /// Leading or trailing whitespace around the handle.
    SurroundingWhitespace,
    /// A valid CID, but not spelled in the one canonical presentation Newt accepts
    /// (an alternate multibase such as base32-UPPER or base58, a bare digest hex, …).
    /// Only the exact base32-lower form the crate renders is accepted.
    NonCanonicalPresentation,
    /// Not a valid frozen-profile CID at all (wrong version/codec/hash/length, or
    /// unparsable). Carries the crate's diagnostic string (`ContentError` is not
    /// `Clone`/`Eq`, so it is captured as text — which is also all a caller needs).
    Profile(String),
}

impl std::fmt::Display for SpillCidError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SurroundingWhitespace => write!(f, "handle has surrounding whitespace"),
            Self::NonCanonicalPresentation => {
                write!(
                    f,
                    "handle is not the canonical base32-lower CID presentation"
                )
            }
            Self::Profile(e) => write!(f, "handle is not a valid content-address: {e}"),
        }
    }
}

impl std::error::Error for SpillCidError {}

/// A spill handle: a validated BLAKE3 CIDv1 wrapping [`ContentId`]. There is NO
/// constructor from an arbitrary string — [`Self::parse`] goes through the crate's
/// frozen `FromStr` AND Newt's canonical-input gate (fail-closed). So "arbitrary
/// strings never become a `SpillCid`".
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SpillCid(ContentId);

impl SpillCid {
    /// Derive the handle from a record (PURE — no store, no allocation).
    pub fn of(record: &SpillRecordV1) -> Result<Self, ContentError> {
        Ok(Self(record.content_id()?))
    }
    /// Parse a handle string under Newt's **canonical-input policy**, which is
    /// deliberately stricter than the crate's frozen profile check. Beyond CIDv1 /
    /// dag-cbor / BLAKE3-256 / 32-byte digest (enforced by `ContentId::from_str`),
    /// Newt requires the EXACT canonical presentation the crate emits — base32-lower,
    /// no surrounding whitespace, no alternate multibase, no bare digest hex.
    ///
    /// This matters because the crate's `FromStr` accepts *any* multibase spelling of
    /// a valid CID (base32-UPPER round-trips through `from_str` but re-renders
    /// lowercase); a spill handle a model pastes back must be the one form Newt ever
    /// emits, so an off-form or whitespace-padded handle is rejected here. (A
    /// canonically-spelled *foreign* CID is still not authorization — that is
    /// mediated by the session store's membership check, not by this parse.)
    pub fn parse(s: &str) -> Result<Self, SpillCidError> {
        if s != s.trim() {
            return Err(SpillCidError::SurroundingWhitespace);
        }
        let cid = ContentId::from_str(s).map_err(|e| SpillCidError::Profile(e.to_string()))?;
        // Canonical-presentation gate: only the exact base32-lower form the crate
        // renders round-trips. Any other valid encoding decodes to the same CID but
        // re-serializes differently ⇒ reject as non-canonical.
        if cid.to_string() != s {
            return Err(SpillCidError::NonCanonicalPresentation);
        }
        Ok(Self(cid))
    }
    /// The canonical handle text (`bafyr4i…`) rendered into a prompt marker.
    pub fn to_handle(self) -> String {
        self.0.to_string()
    }
    pub fn as_content_id(&self) -> &ContentId {
        &self.0
    }
}

impl std::fmt::Display for SpillCid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A staged spill: its content-derived CID + the canonical bytes, computed PURELY
/// (no store, no allocation). Ready to render into a candidate summary; nothing is
/// live until [`SpillStore::commit_batch`] installs it. `Arc<[u8]>` keeps clones
/// cheap while the candidate is passed around.
#[derive(Clone, Debug)]
pub struct StagedSpill {
    cid: SpillCid,
    record: SpillRecordV1,
    canonical_bytes: Arc<[u8]>,
    redacted_chars: usize,
}

impl StagedSpill {
    /// Derive from a record — the store-INDEPENDENT staging step (the CID is a pure
    /// function of the content).
    pub fn from_record(record: SpillRecordV1) -> Result<Self, ContentError> {
        let bytes = record.canonical_form()?;
        let cid = SpillCid(ContentId::from_canonical_bytes(&bytes));
        let redacted_chars = record.redacted_text.chars().count();
        Ok(Self {
            cid,
            record,
            canonical_bytes: Arc::from(bytes.into_boxed_slice()),
            redacted_chars,
        })
    }
    pub fn cid(&self) -> &SpillCid {
        &self.cid
    }
    /// The handle text to render into the candidate summary (`bafyr4i…`).
    pub fn handle(&self) -> String {
        self.cid.to_handle()
    }
    pub fn redacted_chars(&self) -> usize {
        self.redacted_chars
    }
}

/// A committed spill: the CID now resolves in the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommittedSpill {
    cid: SpillCid,
}

impl CommittedSpill {
    pub fn cid(&self) -> &SpillCid {
        &self.cid
    }
}

/// Why a content-addressed spill commit failed — surfaced so callers fail CLOSED.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpillError {
    /// The store lock could not be recovered (a poisoned mutex) — fail CLOSED rather
    /// than commit a partial/empty batch.
    PoisonedStore,
    /// The store already holds this CID with DIFFERENT bytes — a content-addressing
    /// integrity violation (a hash collision or store corruption). Fail CLOSED;
    /// never overwrite one payload with another under the same address.
    IntegrityViolation { cid: String },
    /// The record could not be canonically encoded.
    Encoding(String),
}

/// A content-addressed spill store. `commit_batch` is idempotent (`put_if_absent`)
/// and all-or-none; `fetch` resolves a CID only if it is present in THIS session's
/// store (membership = the read-authorization boundary).
pub trait SpillStore: Send + Sync {
    /// Idempotently install every staged object. Identical bytes under a present CID
    /// are SUCCESS (dedup); DIFFERENT bytes under a present CID are an
    /// [`SpillError::IntegrityViolation`] (fail-closed, no overwrite). ALL-OR-NONE:
    /// prevalidation runs before any install, so a rejected batch installs nothing.
    fn commit_batch(&self, staged: &[StagedSpill]) -> Result<Vec<CommittedSpill>, SpillError>;
    /// Resolve a committed CID to its record (`None` if not present in this session).
    fn fetch(&self, cid: &SpillCid) -> Option<SpillRecordV1>;
    /// Stage + commit one record, returning its handle — the direct (non-candidate)
    /// tool-offload convenience. Idempotent.
    fn store(&self, record: SpillRecordV1) -> Result<SpillCid, SpillError> {
        let staged =
            StagedSpill::from_record(record).map_err(|e| SpillError::Encoding(e.to_string()))?;
        let cid = *staged.cid();
        self.commit_batch(std::slice::from_ref(&staged))?;
        Ok(cid)
    }
    /// Count of UNIQUE committed objects (dedup-aware — one physical object per CID).
    fn unique_objects(&self) -> u64;
    /// Count of committed handle REFERENCES emitted into transcripts (a re-commit of
    /// an already-present CID still counts as a logical reference).
    fn logical_spill_refs(&self) -> u64;
    /// Chars of UNIQUE committed payloads elided from context — counted ONCE per
    /// physical object; a dedup hit does not re-count (§2.10).
    fn unique_offloaded_chars(&self) -> u64;
    /// Chars counted once per logical REFERENCE — a dedup re-commit RE-counts, so this
    /// is the total context pressure the offload relieved across all references. For a
    /// batch of unique payloads it equals [`Self::unique_offloaded_chars`]; identical
    /// content committed twice makes them diverge (§2.10).
    fn logical_offloaded_chars(&self) -> u64;
}

/// In-memory, session-scoped content-addressed store. Keyed by CID text; dedup by
/// construction. Pure (no filesystem) and discarded at session end / `/new`, so
/// orphan-blob GC is trivial — there is no durable blob to reap. The session
/// [`SpillScope`] is stamped onto every record built via [`Self::stage`].
pub struct SessionSpillStore {
    scope: SpillScope,
    map: Mutex<HashMap<String, SpillRecordV1>>,
    unique_objects: AtomicU64,
    logical_refs: AtomicU64,
    unique_offloaded_chars: AtomicU64,
    logical_offloaded_chars: AtomicU64,
}

impl SessionSpillStore {
    /// A store bound to an explicit session nonce (the production caller mints one
    /// random 16-byte nonce at session start; tests pass a fixed one).
    pub fn new(session_nonce: [u8; 16]) -> Self {
        Self {
            scope: SpillScope::Session(session_nonce),
            map: Mutex::new(HashMap::new()),
            unique_objects: AtomicU64::new(0),
            logical_refs: AtomicU64::new(0),
            unique_offloaded_chars: AtomicU64::new(0),
            logical_offloaded_chars: AtomicU64::new(0),
        }
    }

    /// Stamp THIS session's scope onto a record and stage it — the nonce cannot be
    /// forgotten because the store owns it.
    pub fn stage(
        &self,
        provenance: SpillProvenance,
        redacted_text: String,
    ) -> Result<StagedSpill, SpillError> {
        let record = SpillRecordV1::new(self.scope.clone(), provenance, redacted_text);
        StagedSpill::from_record(record).map_err(|e| SpillError::Encoding(e.to_string()))
    }
}

impl SpillStore for SessionSpillStore {
    fn commit_batch(&self, staged: &[StagedSpill]) -> Result<Vec<CommittedSpill>, SpillError> {
        let mut map = self.map.lock().map_err(|_| SpillError::PoisonedStore)?;
        // Prevalidate integrity for the WHOLE batch before installing anything: a CID
        // already present must carry byte-identical content (dedup), never divergent
        // bytes (collision/corruption ⇒ fail closed). All-or-none.
        for s in staged {
            if let Some(existing) = map.get(&s.handle()) {
                let existing_bytes = existing
                    .canonical_form()
                    .map_err(|e| SpillError::Encoding(e.to_string()))?;
                if existing_bytes.as_slice() != &s.canonical_bytes[..] {
                    return Err(SpillError::IntegrityViolation { cid: s.handle() });
                }
            }
        }
        let mut out = Vec::with_capacity(staged.len());
        for s in staged {
            // A vacant CID is a NEW physical object (bump UNIQUE object + chars once);
            // an occupied one is an idempotent dedup hit. Either way it is a logical
            // ref, and its chars count toward the LOGICAL total every time (§2.10).
            if let std::collections::hash_map::Entry::Vacant(e) = map.entry(s.handle()) {
                self.unique_objects.fetch_add(1, Ordering::Relaxed);
                self.unique_offloaded_chars
                    .fetch_add(s.redacted_chars as u64, Ordering::Relaxed);
                e.insert(s.record.clone());
            }
            self.logical_refs.fetch_add(1, Ordering::Relaxed);
            self.logical_offloaded_chars
                .fetch_add(s.redacted_chars as u64, Ordering::Relaxed);
            out.push(CommittedSpill { cid: s.cid });
        }
        Ok(out)
    }

    fn fetch(&self, cid: &SpillCid) -> Option<SpillRecordV1> {
        self.map.lock().ok()?.get(&cid.to_handle()).cloned()
    }

    fn unique_objects(&self) -> u64 {
        self.unique_objects.load(Ordering::Relaxed)
    }

    fn logical_spill_refs(&self) -> u64 {
        self.logical_refs.load(Ordering::Relaxed)
    }

    fn unique_offloaded_chars(&self) -> u64 {
        self.unique_offloaded_chars.load(Ordering::Relaxed)
    }

    fn logical_offloaded_chars(&self) -> u64 {
        self.logical_offloaded_chars.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
impl SessionSpillStore {
    /// Test seam: install a record under an ARBITRARY handle, bypassing content
    /// addressing — to plant a divergent-bytes-under-a-CID corruption that the
    /// [`SpillError::IntegrityViolation`] path must catch.
    fn corrupt_raw_for_test(&self, handle: String, record: SpillRecordV1) {
        self.map.lock().unwrap().insert(handle, record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE_A: [u8; 16] = [7u8; 16];
    const NONCE_B: [u8; 16] = [9u8; 16];

    fn tool_record(nonce: [u8; 16], text: &str) -> SpillRecordV1 {
        SpillRecordV1::new(
            SpillScope::Session(nonce),
            SpillProvenance::ToolOutput {
                tool_name: Some("read_file".into()),
            },
            text.into(),
        )
    }

    #[test]
    fn same_content_and_nonce_converge_on_one_cid() {
        // Idempotent put_if_absent: two identical records → same CID → one object.
        let store = SessionSpillStore::new(NONCE_A);
        let a = StagedSpill::from_record(tool_record(NONCE_A, "same payload")).unwrap();
        let b = StagedSpill::from_record(tool_record(NONCE_A, "same payload")).unwrap();
        assert_eq!(a.cid(), b.cid(), "identical content ⇒ identical CID");
        store.commit_batch(&[a, b]).unwrap();
        assert_eq!(store.unique_objects(), 1, "dedup: one physical object");
        assert_eq!(store.logical_spill_refs(), 2, "two logical references");
    }

    #[test]
    fn different_session_nonce_yields_different_cid() {
        // Equality-leak seal: identical plaintext under two sessions ≠ same address.
        let a = SpillCid::of(&tool_record(NONCE_A, "secret token XYZ")).unwrap();
        let b = SpillCid::of(&tool_record(NONCE_B, "secret token XYZ")).unwrap();
        assert_ne!(
            a, b,
            "different session nonce ⇒ different CID (no equality leak)"
        );
    }

    #[test]
    fn different_content_yields_different_cid() {
        let a = SpillCid::of(&tool_record(NONCE_A, "payload X")).unwrap();
        let b = SpillCid::of(&tool_record(NONCE_A, "payload Y")).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn same_text_different_provenance_yields_different_cid() {
        // Provenance is bound into identity: a tool output and a compaction span with
        // identical text do not collide.
        let tool = SpillRecordV1::new(
            SpillScope::Session(NONCE_A),
            SpillProvenance::ToolOutput { tool_name: None },
            "identical body".into(),
        );
        let span = SpillRecordV1::new(
            SpillScope::Session(NONCE_A),
            SpillProvenance::CompactionSpan,
            "identical body".into(),
        );
        assert_ne!(SpillCid::of(&tool).unwrap(), SpillCid::of(&span).unwrap());
    }

    #[test]
    fn a_committed_cid_resolves_to_its_own_record() {
        let store = SessionSpillStore::new(NONCE_A);
        let rec = tool_record(NONCE_A, "resolve me");
        let staged = StagedSpill::from_record(rec.clone()).unwrap();
        let cid = *staged.cid();
        store.commit_batch(&[staged]).unwrap();
        assert_eq!(
            store.fetch(&cid).as_ref(),
            Some(&rec),
            "handle → its OWN record"
        );
    }

    #[test]
    fn staging_is_pure_a_staged_cid_is_not_fetchable_until_committed() {
        let store = SessionSpillStore::new(NONCE_A);
        let staged = StagedSpill::from_record(tool_record(NONCE_A, "not yet live")).unwrap();
        let cid = *staged.cid();
        assert_eq!(store.fetch(&cid), None, "staging touches no store");
        assert_eq!(store.unique_objects(), 0);
        store.commit_batch(&[staged]).unwrap();
        assert!(store.fetch(&cid).is_some(), "committed ⇒ resolvable");
    }

    #[test]
    fn duplicate_commit_is_idempotent_success() {
        let store = SessionSpillStore::new(NONCE_A);
        let staged = StagedSpill::from_record(tool_record(NONCE_A, "dup")).unwrap();
        store.commit_batch(std::slice::from_ref(&staged)).unwrap();
        store
            .commit_batch(std::slice::from_ref(&staged))
            .expect("a second identical commit is an idempotent success");
        assert_eq!(store.unique_objects(), 1, "still one physical object");
        assert_eq!(store.logical_spill_refs(), 2);
    }

    #[test]
    fn arbitrary_strings_do_not_become_a_spill_cid() {
        assert!(
            SpillCid::parse("not-a-cid").is_err(),
            "malformed → rejected"
        );
        assert!(SpillCid::parse("").is_err());
        // A real handle round-trips (Display → parse).
        let cid = SpillCid::of(&tool_record(NONCE_A, "round trip")).unwrap();
        assert_eq!(SpillCid::parse(&cid.to_handle()).unwrap(), cid);
    }

    #[test]
    fn canonical_input_policy_rejects_noncanonical_and_padded_handles() {
        // §2.2: Newt's canonical-input policy is STRICTER than the crate's profile
        // check. The crate's `FromStr` accepts any multibase spelling of a valid CID
        // (base32-UPPER parses and re-renders lowercase); a spill handle a model
        // pastes back must be the exact form Newt emits.
        let cid = SpillCid::of(&tool_record(NONCE_A, "canonical policy")).unwrap();
        let h = cid.to_handle();
        // The one canonical form parses and round-trips.
        assert_eq!(SpillCid::parse(&h).unwrap(), cid);
        // Surrounding whitespace → rejected explicitly, before profile parsing.
        assert_eq!(
            SpillCid::parse(&format!(" {h}")),
            Err(SpillCidError::SurroundingWhitespace)
        );
        assert_eq!(
            SpillCid::parse(&format!("{h}\n")),
            Err(SpillCidError::SurroundingWhitespace)
        );
        // base32-UPPER is a VALID CID to the crate but NOT Newt's canonical form.
        assert_eq!(
            SpillCid::parse(&h.to_uppercase()),
            Err(SpillCidError::NonCanonicalPresentation)
        );
        // A bare digest hex / gibberish is not a frozen-profile CID at all.
        assert!(matches!(
            SpillCid::parse(&"a".repeat(64)),
            Err(SpillCidError::Profile(_))
        ));
        assert!(matches!(
            SpillCid::parse("not-a-cid"),
            Err(SpillCidError::Profile(_))
        ));
    }

    #[test]
    fn store_convenience_stages_and_commits_with_the_session_scope() {
        let store = SessionSpillStore::new(NONCE_A);
        let staged = store
            .stage(SpillProvenance::CompactionSpan, "scoped".into())
            .unwrap();
        let cid = *staged.cid();
        // The scope stamped is THIS session's — the same as a hand-built record.
        let expect = SpillCid::of(&SpillRecordV1::new(
            SpillScope::Session(NONCE_A),
            SpillProvenance::CompactionSpan,
            "scoped".into(),
        ))
        .unwrap();
        assert_eq!(cid, expect);
        store.commit_batch(&[staged]).unwrap();
        assert!(store.fetch(&cid).is_some());
    }

    #[test]
    fn integrity_violation_on_divergent_bytes_under_the_same_cid() {
        // Plant a corruption: a real CID mapped to a DIFFERENT record. A later commit
        // of the genuine payload under that CID must fail CLOSED, never overwrite.
        let store = SessionSpillStore::new(NONCE_A);
        let genuine = tool_record(NONCE_A, "genuine payload");
        let staged = StagedSpill::from_record(genuine.clone()).unwrap();
        let handle = staged.handle();
        store.corrupt_raw_for_test(handle.clone(), tool_record(NONCE_A, "IMPOSTER"));
        let err = store.commit_batch(&[staged]).unwrap_err();
        assert_eq!(err, SpillError::IntegrityViolation { cid: handle });
    }

    #[test]
    fn a_batch_with_one_integrity_violation_installs_nothing() {
        // All-or-none: a clean spill batched with a corrupted one → whole batch
        // rejected, the clean one is NOT installed.
        let store = SessionSpillStore::new(NONCE_A);
        let clean = StagedSpill::from_record(tool_record(NONCE_A, "clean")).unwrap();
        let clean_cid = *clean.cid();
        let poisoned = StagedSpill::from_record(tool_record(NONCE_A, "poisoned")).unwrap();
        store.corrupt_raw_for_test(poisoned.handle(), tool_record(NONCE_A, "IMPOSTER"));
        assert!(store.commit_batch(&[clean, poisoned]).is_err());
        assert_eq!(
            store.fetch(&clean_cid),
            None,
            "all-or-none: clean not installed"
        );
        assert_eq!(store.unique_objects(), 0);
    }

    #[test]
    fn poisoned_store_fails_closed() {
        let store = SessionSpillStore::new(NONCE_A);
        let staged = StagedSpill::from_record(tool_record(NONCE_A, "x")).unwrap();
        // Poison the mutex (a caught panic while the guard is held).
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = store.map.lock().unwrap();
            panic!("intentional poison");
        }));
        assert_eq!(
            store.commit_batch(&[staged]),
            Err(SpillError::PoisonedStore),
            "a poisoned store fails closed"
        );
    }

    #[test]
    fn metrics_distinguish_unique_from_logical_chars() {
        // §2.10: identical content committed twice → one physical object but two
        // logical references; UNIQUE chars count once, LOGICAL chars re-count the
        // dedup ref.
        let store = SessionSpillStore::new(NONCE_A);
        let text = "twelve chars"; // 12 chars
        let a = StagedSpill::from_record(tool_record(NONCE_A, text)).unwrap();
        let b = StagedSpill::from_record(tool_record(NONCE_A, text)).unwrap();
        store.commit_batch(&[a, b]).unwrap();
        assert_eq!(store.unique_objects(), 1);
        assert_eq!(store.logical_spill_refs(), 2);
        assert_eq!(
            store.unique_offloaded_chars(),
            12,
            "unique chars counted once"
        );
        assert_eq!(
            store.logical_offloaded_chars(),
            24,
            "logical chars re-count the dedup ref"
        );
    }

    #[test]
    fn a_foreign_session_cid_is_indistinguishable_from_unknown() {
        // §2.4: a CID minted under ANOTHER session's nonce is not fetchable here — it
        // returns the SAME None as a never-committed CID, so cross-session existence
        // is never leaked.
        let mine = SessionSpillStore::new(NONCE_A);
        let foreign = SpillCid::of(&tool_record(NONCE_B, "foreign secret")).unwrap();
        let unknown = SpillCid::of(&tool_record(NONCE_A, "never committed")).unwrap();
        assert_eq!(mine.fetch(&foreign), None, "foreign-session CID: not found");
        assert_eq!(mine.fetch(&unknown), None, "unknown CID: not found");
        assert_eq!(
            mine.fetch(&foreign),
            mine.fetch(&unknown),
            "identical externally-visible result — no existence leak"
        );
    }

    #[test]
    fn a_planted_secret_never_appears_in_canonical_bytes_or_stored_record() {
        // §2.3: redaction happens BEFORE a record is built (the type only ever holds
        // redacted_text), so a raw secret never reaches the canonical bytes, the CID
        // input, the stored record, or a fetched record. Grounds redact-on-store at
        // the record boundary; the raw→redact→record pipeline is wired at the producer
        // sites (§2.7).
        const SECRET: &str = "sk-PLANTEDsecret0123456789";
        let redacted = "head [REDACTED] tail".to_string();
        assert!(!redacted.contains(SECRET));
        let record = tool_record(NONCE_A, &redacted);
        let bytes = record.canonical_form().unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains(SECRET),
            "raw secret absent from canonical CID input bytes"
        );
        let store = SessionSpillStore::new(NONCE_A);
        let cid = store.store(record).unwrap();
        let got = store.fetch(&cid).unwrap();
        assert!(
            !got.redacted_text.contains(SECRET),
            "raw secret absent from the stored record"
        );
        assert!(
            !cid.to_handle().contains(SECRET),
            "raw secret absent from the handle"
        );
    }
}
