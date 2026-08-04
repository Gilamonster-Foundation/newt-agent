/- #1528 B3 — the SPILL-CAPABILITY kernel.

   An abstract model of the transactional compaction spill store: `reserve` (the
   store allocates a stable id), `commit` (install a payload under a reserved id),
   `fetch` (resolve an id), and `renderHandle` (a compaction span emits a handle
   ONLY when there is a real store). `lake build` machine-checks the capability laws
   the Rust `SpillStore` + `CandidateSpillStore` (#1528 B3, corrections 1–7) rely on:

     no store ⟹ no handle
     a rendered handle IS a store reservation (never predicted)
     a reserved id is not fetchable until committed (a rejected candidate = no record)
     commit installs the payload under EXACTLY the reserved id
     a committed handle resolves to its own payload
     reserve does NOT increment the committed count
     reject does NOT increment the committed count
     only commit increments the committed count

   This is the pure single-thread capability algebra. Concurrency BINDING (an
   interleaved external write cannot rebind a reserved id) is a Rust behavioral test
   and a future TLA+ obligation — NOT claimed here. No Mathlib; bare toolchain;
   sorry-free. -/
namespace NewtPolicy.CompactionSpill

abbrev Id := Nat
abbrev Payload := Nat

/-- The store as a fetch function + an id allocator + a committed count. `fetchFn`
    resolves committed ids; `allocNext` is the next id `reserve` hands out;
    `committedCount` is what `spills()` reports. -/
structure Store where
  fetchFn : Id → Option Payload
  allocNext : Id
  committedCount : Nat

def Store.empty : Store :=
  { fetchFn := fun _ => none, allocNext := 0, committedCount := 0 }

/-- Reserve a fresh, store-allocated id; bumps the allocator ONLY (not the committed
    count, not the fetch map). -/
def Store.reserve (s : Store) : Id × Store :=
  (s.allocNext, { s with allocNext := s.allocNext + 1 })

/-- Install `p` under the reserved `id` (COMMIT) — the only step that makes an id
    fetchable and the only step that increments the committed count. -/
def Store.commit (s : Store) (id : Id) (p : Payload) : Store :=
  { s with
    fetchFn := fun i => if i = id then some p else s.fetchFn i
    committedCount := s.committedCount + 1 }

def Store.fetch (s : Store) (id : Id) : Option Payload := s.fetchFn id

/-- Well-formedness: every id at-or-beyond the allocator is unfetchable, so a fresh
    reservation's id (= `allocNext`) is never already committed. Preserved by every
    operation. -/
def WF (s : Store) : Prop := ∀ id, s.allocNext ≤ id → s.fetchFn id = none

theorem wf_empty : WF Store.empty := by intro id _; rfl

theorem reserve_preserves_wf (s : Store) (h : WF s) : WF (s.reserve).2 := by
  intro id hid
  -- `(s.reserve).2.allocNext` is DEFEQ `s.allocNext + 1`; coerce and use it.
  have hle : s.allocNext + 1 ≤ id := hid
  exact h id (Nat.le_of_succ_le hle)

theorem commit_preserves_wf (s : Store) (id : Id) (p : Payload)
    (h : WF s) (hid : id < s.allocNext) : WF (s.commit id p) := by
  intro i hi
  have hle : s.allocNext ≤ i := hi
  have hne : i ≠ id := Nat.ne_of_gt (Nat.lt_of_lt_of_le hid hle)
  show (if i = id then some p else s.fetchFn i) = none
  rw [if_neg hne]
  exact h i hle

/-! ### The capability laws. -/

/-- reserve does NOT increment the committed count. -/
theorem reserve_does_not_increment_committed (s : Store) :
    (s.reserve).2.committedCount = s.committedCount := rfl

/-- reject (drop the reservation, never commit) does NOT increment the committed
    count — it is exactly the reserved store's count. -/
theorem reject_does_not_increment_committed (s : Store) :
    (s.reserve).2.committedCount = s.committedCount := rfl

/-- ONLY commit increments the committed count. -/
theorem commit_increments_committed (s : Store) (id : Id) (p : Payload) :
    (s.commit id p).committedCount = s.committedCount + 1 := rfl

/-- A committed handle resolves to its OWN payload (commit installs `p` under exactly
    the committed id). -/
theorem committed_handle_resolves (s : Store) (id : Id) (p : Payload) :
    (s.commit id p).fetch id = some p := by
  simp [Store.commit, Store.fetch]

/-- A fresh reservation's id is NOT fetchable until committed — so a rejected
    candidate (reserve, never commit) leaves no fetchable record. -/
theorem reserved_id_not_fetchable (s : Store) (h : WF s) :
    (s.reserve).2.fetch (s.reserve).1 = none := by
  show s.fetchFn s.allocNext = none
  exact h s.allocNext (Nat.le_refl _)

/-- reserve → commit binds the payload under the SAME store-issued id (commit
    preserves reserved handle identity). -/
theorem reserve_then_commit_resolves (s : Store) (p : Payload) :
    ((s.reserve).2.commit (s.reserve).1 p).fetch (s.reserve).1 = some p := by
  simp [Store.reserve, Store.commit, Store.fetch]

/-! ### Handle provenance (correction 1). -/

/-- Render a retrieval handle for a compaction span: a handle IS a store
    reservation, so there is NONE without a store. -/
def renderHandle : Option Store → Option Id
  | none => none
  | some s => some (s.reserve).1

/-- NO store ⟹ NO handle. -/
theorem no_store_no_handle : renderHandle none = none := rfl

/-- A rendered handle is exactly a store-issued reservation id (never predicted). -/
theorem handle_requires_reservation (s : Store) :
    renderHandle (some s) = some (s.reserve).1 := rfl

/-! ### Non-vacuity demonstration. -/

/-- If `commit` did NOT install the payload under the reserved id (a regression),
    `committed_handle_resolves` would break. Concretely: a fresh empty store commits
    payload `7` under id `0` and fetches exactly `7`. -/
example : (Store.empty.commit 0 7).fetch 0 = some 7 := by
  simp [Store.empty, Store.commit, Store.fetch]

/-- A reserved-but-uncommitted id resolves to nothing (from the well-formed empty
    store). -/
example : (Store.empty.reserve).2.fetch (Store.empty.reserve).1 = none :=
  reserved_id_not_fetchable Store.empty wf_empty

end NewtPolicy.CompactionSpill
