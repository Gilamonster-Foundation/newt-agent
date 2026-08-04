//! Tool-output offloading — the `tool_offload` context feature (Step 26.3, #584).
//!
//! When a single tool result exceeds [`TOOL_RESULT_SPILL_CAP`] **and** the
//! feature is on, the FULL payload is redacted via [`redact_secrets`] and stored
//! in a session [`SpillStore`] keyed by a short id; a head+tail excerpt plus a
//! `spill:<id>` handle is injected into context in its place. The model re-reads
//! the full (redacted) payload via `memory_fetch("spill:<id>")`.
//!
//! **Redact-on-store (the security contract):** the raw result is redacted
//! BEFORE anything is stored or shown, and the un-redacted string is dropped
//! immediately after — only the redacted copy is ever retained or displayed, so
//! no raw secret reaches disk-or-context. `redact_secrets` is a closed,
//! high-precision pattern table (it won't catch novel secret shapes — the same
//! accepted limitation as the summarizer path).

use crate::agentic::compress::redact_secrets;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Offload trigger: a tool result longer than this many chars spills. ~4k tokens
/// at the codebase's chars/4 heuristic (cf. `SUMMARY_INPUT_MSG_CAP` = 2_000).
pub const TOOL_RESULT_SPILL_CAP: usize = 16_000;

/// Chars kept from the head / tail of an offloaded payload in the teaser. Kept
/// well under [`TOOL_RESULT_SPILL_CAP`] so the teaser can never re-overflow.
const HEAD_CHARS: usize = 800;
const TAIL_CHARS: usize = 800;

/// Why a spill commit failed (#1528 B3). A capability-level fault, surfaced so the
/// compaction helper can fail CLOSED — never leaving a live retrieval handle
/// unresolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpillCommitError {
    /// The id is already committed. Commits are single-shot and committed payloads
    /// are IMMUTABLE (BHV-SPILL-003/005) — this includes an intra-batch duplicate.
    DuplicateCommit,
    /// The candidate transaction log could not be recovered (a poisoned lock) — fail
    /// CLOSED rather than commit an empty/partial transaction (BHV-SPILL-007).
    PoisonedTransaction,
}

/// An UNFORGEABLE, single-use spill CAPABILITY (#1528 B3, BHV-SPILL-002/003/004).
/// Issued ONLY by [`SpillStore::reserve`] — there is no public constructor, so a
/// caller cannot fabricate a reservation for an arbitrary id. It is BOUND to its
/// issuing store (its [`Self::commit`] installs into THAT store; it cannot be handed
/// to a different one) and CONSUMED by commit (`self: Box<Self>` — a reservation
/// commits at most once; a second commit does not typecheck).
pub trait SpillReservation: Send {
    /// The store-issued id to render into the candidate summary — the SAME id the
    /// payload is installed under at commit. Never predicted.
    fn id(&self) -> &str;
    /// Install `redacted` under this reservation's id, in its ISSUING store. Consumes
    /// the capability. Fails CLOSED on a duplicate id (`DuplicateCommit`, preserving
    /// the existing record) or a poisoned candidate log (`PoisonedTransaction`).
    fn commit(self: Box<Self>, redacted: String) -> Result<(), SpillCommitError>;
}

/// A session store for offloaded / evicted (already-redacted) payloads. Methods take
/// `&self` (interior mutability) so a single shared `&dyn SpillStore` serves BOTH the
/// loop's write path and the `memory_fetch` read path without the `&mut dyn _`
/// reborrow/invariance dance. #1528 B3 makes writes go through an UNFORGEABLE
/// [`SpillReservation`] capability so a spill can be STAGED (id allocated by the
/// store) and committed transactionally.
pub trait SpillStore: Send + Sync {
    /// Issue a fresh, unforgeable reservation — the STORE owns id allocation. Commit
    /// or drop the returned capability; a dropped reservation installs nothing.
    fn reserve(&self) -> Box<dyn SpillReservation + '_>;
    /// Reserve + commit in one; returns the committed id. A fresh reservation cannot
    /// be a duplicate, and the direct (non-candidate) store path has no candidate log
    /// to poison, so this is the id-returning convenience for the tool-offload path.
    fn store(&self, redacted: String) -> String {
        let reservation = self.reserve();
        let id = reservation.id().to_string();
        let _ = reservation.commit(redacted);
        id
    }
    /// Fetch a stored payload by id (`None` if unknown / uncommitted / expired).
    fn fetch(&self, id: &str) -> Option<String>;
    /// Number of COMMITTED payloads this session (reservations excluded).
    fn spills(&self) -> u64;
    /// Total chars of COMMITTED payloads elided from context.
    fn offloaded_chars(&self) -> u64;
}

/// In-memory, session-scoped [`SpillStore`] — pure (no filesystem), discarded at
/// session end / `/new`. Ids are monotonic (`s0`, `s1`, …) so injected handles
/// are deterministic and unit-testable (no uuid, no clock). Allocation
/// (`alloc_counter`, bumped by `reserve`) is split from the committed count
/// (`committed`, bumped only by a successful install), so a reservation reserves a
/// unique id without counting as a spill.
#[derive(Default)]
pub struct SessionSpillStore {
    map: Mutex<HashMap<String, String>>,
    alloc_counter: AtomicU64,
    committed: AtomicU64,
    offloaded_chars: AtomicU64,
}

impl SessionSpillStore {
    /// Install a reserved payload IMMUTABLY (BHV-SPILL-005): a VACANT id installs and
    /// bumps the counts ONCE; an OCCUPIED id returns `DuplicateCommit` and preserves
    /// the existing payload AND the counts. Called ONLY by a [`SessionReservation`]
    /// this store issued.
    fn install(&self, id: String, redacted: String) -> Result<(), SpillCommitError> {
        let mut map = self
            .map
            .lock()
            .map_err(|_| SpillCommitError::PoisonedTransaction)?;
        if map.contains_key(&id) {
            return Err(SpillCommitError::DuplicateCommit);
        }
        self.offloaded_chars
            .fetch_add(redacted.chars().count() as u64, Ordering::Relaxed);
        map.insert(id, redacted);
        self.committed.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// The store-issued reservation capability. Holds a reference to its issuing store,
/// so [`SpillReservation::commit`] can only install THERE.
struct SessionReservation<'a> {
    store: &'a SessionSpillStore,
    id: String,
}

impl SpillReservation for SessionReservation<'_> {
    fn id(&self) -> &str {
        &self.id
    }
    fn commit(self: Box<Self>, redacted: String) -> Result<(), SpillCommitError> {
        self.store.install(self.id, redacted)
    }
}

impl SpillStore for SessionSpillStore {
    fn reserve(&self) -> Box<dyn SpillReservation + '_> {
        // Atomic bump → a globally unique id even under concurrent reservations /
        // stores; NOT counted as a committed spill.
        let n = self.alloc_counter.fetch_add(1, Ordering::Relaxed);
        Box::new(SessionReservation {
            store: self,
            id: format!("s{n}"),
        })
    }

    fn fetch(&self, id: &str) -> Option<String> {
        self.map.lock().ok().and_then(|m| m.get(id).cloned())
    }

    fn spills(&self) -> u64 {
        self.committed.load(Ordering::Relaxed)
    }

    fn offloaded_chars(&self) -> u64 {
        self.offloaded_chars.load(Ordering::Relaxed)
    }
}

/// The teaser injected in place of an offloaded payload: head + a re-read marker
/// + tail. Already-redacted input; kept short so it cannot re-overflow.
fn head_tail_excerpt(redacted: &str, id: &str) -> String {
    let chars: Vec<char> = redacted.chars().collect();
    let total = chars.len();
    let head: String = chars.iter().take(HEAD_CHARS).collect();
    let tail: String = chars
        .iter()
        .skip(total.saturating_sub(TAIL_CHARS))
        .collect();
    format!(
        "{head}\n\n[… tool output truncated: {total} chars offloaded. Use \
         memory_fetch(\"spill:{id}\") to read the full (secret-redacted) payload …]\n\n{tail}"
    )
}

/// Offload an oversized tool result (Step 26.3). Returns `result` UNCHANGED when
/// the feature is off, no spill store is provided, or the result is under the
/// cap (the bit-for-bit OFF path). Otherwise redacts → stores → returns a
/// head+tail teaser carrying the `spill:<id>` handle. The raw `result` is
/// consumed and dropped; only its redacted form is retained or shown.
pub fn maybe_offload(result: String, tool_offload: bool, spill: Option<&dyn SpillStore>) -> String {
    let Some(store) = spill else {
        return result;
    };
    if !tool_offload || result.chars().count() <= TOOL_RESULT_SPILL_CAP {
        return result;
    }
    let redacted = redact_secrets(&result);
    let id = store.store(redacted.clone());
    head_tail_excerpt(&redacted, &id)
}

/// Redact and store a full payload, returning `(id, redacted_payload)` so a
/// caller can build its own model-facing teaser from the exact bytes that were
/// stored. Used by `run_command` before its model-facing cap, so the spill store
/// sees the true tail instead of an already-truncated result.
pub fn store_redacted_full(result: &str, spill: &dyn SpillStore) -> (String, String) {
    let redacted = redact_secrets(result);
    let id = spill.store(redacted.clone());
    (id, redacted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_store_round_trips_with_monotonic_ids() {
        let s = SessionSpillStore::default();
        let id0 = s.store("alpha".to_string());
        let id1 = s.store("beta".to_string());
        assert_eq!(id0, "s0");
        assert_eq!(id1, "s1");
        assert_eq!(s.fetch("s0").as_deref(), Some("alpha"));
        assert_eq!(s.fetch("s1").as_deref(), Some("beta"));
        assert_eq!(s.fetch("s99"), None, "unknown id → None, no panic");
        assert_eq!(s.spills(), 2);
        assert_eq!(s.offloaded_chars(), 9); // "alpha"(5) + "beta"(4)
    }

    #[test]
    fn reservation_allocates_a_unique_id_but_does_not_count_until_committed() {
        // #1528 B3: reserve() allocates a unique id (store-owned) but is NOT a
        // committed spill and NOT fetchable until commit_reserved; a dropped
        // (rejected) reservation leaves no record and does not bump the count.
        let s = SessionSpillStore::default();
        let r0 = s.reserve();
        let r1 = s.reserve();
        let (id0, id1) = (r0.id().to_string(), r1.id().to_string());
        assert_ne!(id0, id1, "reservations get unique ids");
        assert_eq!(
            s.spills(),
            0,
            "reserve does NOT increment the committed count"
        );
        assert_eq!(s.fetch(&id0), None, "a reservation is not fetchable");
        // Commit r0 (consumes the capability); reject (drop) r1.
        r0.commit("committed payload".to_string())
            .expect("a fresh reservation commits");
        drop(r1);
        assert_eq!(s.spills(), 1, "only commit increments the committed count");
        assert_eq!(s.fetch(&id0).as_deref(), Some("committed payload"));
        assert_eq!(
            s.fetch(&id1),
            None,
            "a rejected reservation leaves no record"
        );
        // A later store() must not reuse the retired id1 (allocator is monotonic).
        let id2 = s.store("later".to_string());
        assert_ne!(id2, id1, "a retired reservation id is never reused");
    }

    #[test]
    fn committed_id_is_immutable_a_duplicate_install_is_rejected() {
        // BHV-SPILL-005: a committed id can NOT be overwritten. Fails on 5975d64,
        // whose commit blind-`insert`ed. White-box: `install` is the internal,
        // id-checked commit path every reservation drives.
        let s = SessionSpillStore::default();
        let a = s.store("A".to_string()); // s0 = A
        assert_eq!(
            s.install(a.clone(), "B".to_string()),
            Err(SpillCommitError::DuplicateCommit),
            "a duplicate/forged install for a committed id is rejected"
        );
        assert_eq!(
            s.fetch(&a).as_deref(),
            Some("A"),
            "existing payload preserved"
        );
        assert_eq!(s.spills(), 1, "committed count unchanged");
        assert_eq!(s.offloaded_chars(), 1, "char count unchanged (only 'A')");
    }

    #[test]
    fn a_reservation_commits_into_its_issuing_store_only() {
        // BHV-SPILL-004: a reservation is BOUND to its issuing store — its commit
        // installs THERE, and no other store gains a record. (Handing the token to a
        // different store is UNREPRESENTABLE: `commit(self)` takes no store argument.)
        let a = SessionSpillStore::default();
        let b = SessionSpillStore::default();
        let r = a.reserve();
        let id = r.id().to_string();
        r.commit("payload".to_string()).unwrap();
        assert_eq!(
            a.fetch(&id).as_deref(),
            Some("payload"),
            "installed in the ISSUING store"
        );
        assert_eq!(b.fetch(&id), None, "no foreign store gains a record");
        assert_eq!(b.spills(), 0);
    }

    #[test]
    fn maybe_offload_truth_table() {
        let big = "x".repeat(TOOL_RESULT_SPILL_CAP + 1);

        // (a) feature OFF + over-cap → unchanged, store untouched
        let s = SessionSpillStore::default();
        assert_eq!(maybe_offload(big.clone(), false, Some(&s)), big);
        assert_eq!(s.spills(), 0);

        // (b) no store + over-cap → unchanged, no panic
        assert_eq!(maybe_offload(big.clone(), true, None), big);

        // (c) ON + Some + UNDER cap → unchanged, store untouched
        let s = SessionSpillStore::default();
        let small = "x".repeat(TOOL_RESULT_SPILL_CAP); // == cap is NOT over
        assert_eq!(maybe_offload(small.clone(), true, Some(&s)), small);
        assert_eq!(s.spills(), 0);

        // (d) ON + Some + OVER cap → teaser with spill: handle, shorter, stored
        let s = SessionSpillStore::default();
        let out = maybe_offload(big.clone(), true, Some(&s));
        assert!(
            out.contains("spill:s0"),
            "teaser carries the handle: {out:.80}"
        );
        assert!(out.contains("memory_fetch"), "teaser coaches re-read");
        assert!(
            out.chars().count() < big.chars().count(),
            "teaser is shorter"
        );
        assert_eq!(s.spills(), 1);
        // the STORED value is the redacted full payload (here no secret → == big)
        assert_eq!(s.fetch("s0").as_deref(), Some(big.as_str()));
    }

    #[test]
    fn offload_redacts_before_store_and_in_teaser() {
        // A planted secret in an over-cap payload must never survive raw.
        let secret = "sk-ABCDEFGHIJKLMNOPQRST0123";
        let payload = format!(
            "{}\n{secret}\n{}",
            "head ".repeat(2_000),
            "tail ".repeat(2_000)
        );
        assert!(payload.chars().count() > TOOL_RESULT_SPILL_CAP);
        let s = SessionSpillStore::default();
        let teaser = maybe_offload(payload, true, Some(&s));
        let stored = s.fetch("s0").expect("payload was stored");
        assert!(stored.contains("[REDACTED]"), "stored payload is redacted");
        assert!(
            !stored.contains(secret),
            "raw secret NOT retained in the store"
        );
        assert!(
            !teaser.contains(secret),
            "raw secret NOT shown in the teaser"
        );
    }
}
