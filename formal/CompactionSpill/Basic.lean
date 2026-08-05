/- #1528 B3 — the SPILL-CAPABILITY kernel (reservation-aware).

   An abstract model of the transactional compaction spill store as an UNFORGEABLE,
   STORE-BOUND, SINGLE-USE capability. A `Reservation` carries the `storeId` of the
   store that issued it and the reserved `rid`; a `Store` tracks the set of
   `outstanding` (reserved, not-yet-resolved) ids, a fetch map of committed payloads,
   an id allocator, and a committed count. There is no way to build a `Reservation`
   whose `rid` a store considers `outstanding` except by calling `reserve` — mirroring
   the Rust design where `SpillReservation` has no public constructor and is issued
   ONLY by `SpillStore::reserve`.

   `lake build` machine-checks the capability laws the Rust `SpillReservation` +
   `SpillStore` + `CandidateSpillStore` (#1528 B3, capability round) rely on:

     reserve issues a store-BOUND id and does NOT increment the committed count
     a FOREIGN reservation (wrong storeId) is rejected — commit is store-bound
     an UNRESERVED id is rejected — no arbitrary-id insertion
     commit installs the payload under EXACTLY the reserved id …
     … and PRESERVES every existing payload (no clobber)
     commit CONSUMES the reservation (its id leaves `outstanding`) …
     … so a committed reservation CANNOT commit again (single-use)
     reject retires the reservation WITHOUT a commit or a count bump
     a FAILED commit does not increment the counts; ONLY a successful one does
     no store ⟹ no handle (a handle IS a reservation)

   This is the pure single-thread capability algebra. BATCH all-or-none atomicity of the
   candidate staging store is a Rust behavioral test
   (`candidate_commit_is_all_or_none_on_a_duplicate_id`) and a TLA+ obligation
   (`CandidateBatchCommit`) — deliberately NOT claimed here. Concurrency BINDING (an
   interleaved external write cannot rebind a reserved id) is likewise a Rust test
   (`candidate_spill_store_binds_reserved_ids_under_an_interleaved_external_write`) plus
   a TLA+ obligation. No Mathlib; bare toolchain; sorry-free. -/
namespace NewtPolicy.CompactionSpill

abbrev Id := Nat
abbrev Payload := Nat
abbrev StoreId := Nat

/-- An unforgeable capability: WHICH store issued it (`storeId`) + the reserved id. -/
structure Reservation where
  storeId : StoreId
  rid : Id

/-- The store: its identity, the outstanding (reserved-not-resolved) ids, the committed
    fetch map, the id allocator, and the committed count. -/
structure Store where
  storeId : StoreId
  outstanding : Id → Bool
  fetchFn : Id → Option Payload
  allocNext : Id
  committedCount : Nat

def Store.fetch (s : Store) (id : Id) : Option Payload := s.fetchFn id

/-- Reserve a fresh, store-BOUND id: mark it outstanding, bump the allocator. Does NOT
    touch the fetch map or the committed count. -/
def Store.reserve (s : Store) : Reservation × Store :=
  ( { storeId := s.storeId, rid := s.allocNext },
    { s with
      outstanding := fun i => (i == s.allocNext) || s.outstanding i
      allocNext := s.allocNext + 1 } )

/-- Why a commit was rejected. -/
inductive CommitErr where
  | foreign      -- reservation issued by a DIFFERENT store
  | unreserved   -- id not outstanding (never reserved, or already resolved)

/-- COMMIT: install `p` under the reserved id — but ONLY for a MATCHING, OUTSTANDING
    reservation. Store-bound (foreign ⇒ error) and single-use (a resolved id is no
    longer outstanding ⇒ error). On success: installs, retires the reservation from
    `outstanding`, bumps the committed count. -/
def Store.commit (s : Store) (r : Reservation) (p : Payload) : Except CommitErr Store :=
  if r.storeId == s.storeId then
    if s.outstanding r.rid then
      .ok { s with
        fetchFn := fun i => if i == r.rid then some p else s.fetchFn i
        outstanding := fun i => if i == r.rid then false else s.outstanding i
        committedCount := s.committedCount + 1 }
    else
      .error .unreserved
  else
    .error .foreign

/-- A FAILED commit leaves the store UNCHANGED (the caller keeps `s`). -/
def Store.tryCommit (s : Store) (r : Reservation) (p : Payload) : Store :=
  match s.commit r p with
  | .ok s' => s'
  | .error _ => s

/-- REJECT: retire the reservation WITHOUT installing or counting. -/
def Store.reject (s : Store) (r : Reservation) : Store :=
  { s with outstanding := fun i => if i == r.rid then false else s.outstanding i }

/-! ### The capability laws. -/

/-- reserve issues an id BOUND to the issuing store. -/
theorem reserve_is_store_bound (s : Store) :
    (s.reserve).1.storeId = s.storeId := rfl

/-- reserve marks the freshly-issued id OUTSTANDING (so its own commit will succeed). -/
theorem reserve_marks_id_outstanding (s : Store) :
    (s.reserve).2.outstanding (s.reserve).1.rid = true := by
  simp [Store.reserve]

/-- reserve does NOT increment the committed count. -/
theorem reserve_does_not_increment_committed_count (s : Store) :
    (s.reserve).2.committedCount = s.committedCount := rfl

/-- A FOREIGN reservation (wrong `storeId`) is rejected — commit is store-bound. -/
theorem foreign_reservation_rejected (s : Store) (r : Reservation) (p : Payload)
    (h : r.storeId ≠ s.storeId) : s.commit r p = .error .foreign := by
  have hb : (r.storeId == s.storeId) = false := by
    cases h' : r.storeId == s.storeId with
    | false => rfl
    | true => exact absurd (eq_of_beq h') h
  simp [Store.commit, hb]

/-- An UNRESERVED id is rejected — no arbitrary-id insertion (even with a matching
    `storeId`, an id that is not outstanding cannot be committed). -/
theorem unreserved_id_rejected (s : Store) (r : Reservation) (p : Payload)
    (hmatch : r.storeId = s.storeId) (h : s.outstanding r.rid = false) :
    s.commit r p = .error .unreserved := by
  simp [Store.commit, hmatch, h]

/-- commit installs the payload under EXACTLY the reserved id. -/
theorem commit_installs_exact_reserved_payload (s : Store) (r : Reservation) (p : Payload)
    (hmatch : r.storeId = s.storeId) (hout : s.outstanding r.rid = true) :
    (s.tryCommit r p).fetch r.rid = some p := by
  simp [Store.tryCommit, Store.commit, hmatch, hout, Store.fetch]

/-- commit PRESERVES every existing payload (no clobber of a different id). -/
theorem commit_preserves_existing_payloads (s : Store) (r : Reservation) (p : Payload)
    (j : Id) (q : Payload)
    (hmatch : r.storeId = s.storeId) (hout : s.outstanding r.rid = true)
    (hjne : j ≠ r.rid) (hj : s.fetch j = some q) :
    (s.tryCommit r p).fetch j = some q := by
  simpa [Store.tryCommit, Store.commit, hmatch, hout, Store.fetch, hjne] using hj

/-- commit CONSUMES the reservation: its id leaves `outstanding`. -/
theorem commit_consumes_reservation (s : Store) (r : Reservation) (p : Payload)
    (hmatch : r.storeId = s.storeId) (hout : s.outstanding r.rid = true) :
    (s.tryCommit r p).outstanding r.rid = false := by
  simp [Store.tryCommit, Store.commit, hmatch, hout]

/-- commit PRESERVES the store identity (so the store-binding of OTHER reservations is
    unchanged — the consumed reservation is the only thing retired). -/
theorem commit_preserves_store_id (s : Store) (r : Reservation) (p : Payload)
    (hmatch : r.storeId = s.storeId) (hout : s.outstanding r.rid = true) :
    (s.tryCommit r p).storeId = s.storeId := by
  simp [Store.tryCommit, Store.commit, hmatch, hout]

/-- … so a committed reservation CANNOT commit again (single-use): re-presenting the
    now-retired reservation is rejected as unreserved. -/
theorem committed_reservation_cannot_commit_again (s : Store) (r : Reservation)
    (p p2 : Payload) (hmatch : r.storeId = s.storeId) (hout : s.outstanding r.rid = true) :
    (s.tryCommit r p).commit r p2 = .error .unreserved :=
  unreserved_id_rejected (s.tryCommit r p) r p2
    (hmatch.trans (commit_preserves_store_id s r p hmatch hout).symm)
    (commit_consumes_reservation s r p hmatch hout)

/-- reject retires the reservation WITHOUT a commit, a count bump, or a new record. -/
theorem reject_removes_reservation_without_commit (s : Store) (r : Reservation) :
    (s.reject r).outstanding r.rid = false
      ∧ (s.reject r).committedCount = s.committedCount
      ∧ (s.reject r).fetchFn = s.fetchFn := by
  refine ⟨?_, rfl, rfl⟩
  simp [Store.reject]

/-- A FAILED commit does not increment the counts (the store is returned unchanged). -/
theorem failed_commit_does_not_increment_counts (s : Store) (r : Reservation) (p : Payload)
    (e : CommitErr) (h : s.commit r p = .error e) :
    (s.tryCommit r p).committedCount = s.committedCount := by
  simp [Store.tryCommit, h]

/-- ONLY a successful commit increments the committed count (by exactly one). -/
theorem only_successful_commit_increments_counts (s : Store) (r : Reservation) (p : Payload)
    (hmatch : r.storeId = s.storeId) (hout : s.outstanding r.rid = true) :
    (s.tryCommit r p).committedCount = s.committedCount + 1 := by
  simp [Store.tryCommit, Store.commit, hmatch, hout]

/-- The full capability flow: reserve then commit resolves the payload under the
    store-issued id (end-to-end, NO validity hypothesis needed — reserve establishes
    both the store-binding and the outstanding-ness commit requires). -/
theorem reserve_then_commit_resolves (s : Store) (p : Payload) :
    ((s.reserve).2.tryCommit (s.reserve).1 p).fetch (s.reserve).1.rid = some p := by
  apply commit_installs_exact_reserved_payload
  · rfl
  · exact reserve_marks_id_outstanding s

/-! ### Handle provenance (correction 1). -/

/-- Render a retrieval handle for a compaction span: a handle IS a store reservation,
    so there is NONE without a store. -/
def renderHandle : Option Store → Option Id
  | none => none
  | some s => some (s.reserve).1.rid

/-- NO store ⟹ NO handle. -/
theorem no_store_no_handle : renderHandle none = none := rfl

/-- A rendered handle is exactly a store-issued reservation id (never predicted). -/
theorem handle_requires_reservation (s : Store) :
    renderHandle (some s) = some (s.reserve).1.rid := rfl

/-! ### Non-vacuity demonstrations. -/

/-- A concrete store: identity `7`, nothing outstanding, empty fetch map. -/
def demoStore : Store :=
  { storeId := 7, outstanding := fun _ => false, fetchFn := fun _ => none,
    allocNext := 0, committedCount := 0 }

/-- reserve → commit on a concrete store installs and resolves the payload. -/
example :
    (((demoStore.reserve).2).tryCommit (demoStore.reserve).1 42).fetch
        (demoStore.reserve).1.rid = some 42 := rfl

/-- A FOREIGN reservation (a fabricated `storeId`) is rejected even though its `rid`
    LOOKS like a valid, freshly-allocated id — store-binding is not spoofable. -/
example : demoStore.commit { storeId := 99, rid := 0 } 5 = .error .foreign := rfl

/-- An UNRESERVED id under the RIGHT store is still rejected — arbitrary-id insertion
    is impossible (id `3` was never reserved on `demoStore`). -/
example : demoStore.commit { storeId := 7, rid := 3 } 5 = .error .unreserved := rfl

end NewtPolicy.CompactionSpill
