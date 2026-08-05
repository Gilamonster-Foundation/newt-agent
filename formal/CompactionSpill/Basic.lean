/- #1528 B3 — the CONTENT-ADDRESSED spill store kernel.

   An abstract model of the compaction/tool-offload spill store as a
   CONTENT-ADDRESSED, session-scoped object store — replacing the OLD
   reservation/allocator model (now deleted). Mirrors the Rust design in
   `newt-core/src/agentic/content_spill.rs`, where a spill handle is the CID of a
   versioned, session-scoped record — NOT an allocated id.

   A `Record` bundles `schema`, `nonce` (the per-session privacy scope), a
   `provenance` tag (0 = ToolOutput, 1 = CompactionSpan), and the redacted `text`.
   All fields are `Nat` — the model lives over a finite domain, with deterministic
   equality (`deriving DecidableEq`). The CID is modelled as a DETERMINISTIC function
   `cidf : Record → Cid`.

   ## The injective-CID assumption (HONEST — this is NOT a BLAKE3 proof)

   The Rust CID is the BLAKE3 CIDv1 of the canonical dag-cbor of the record. Two
   properties matter: (1) it is a DETERMINISTIC function of the content (equal
   records ⇒ equal bytes ⇒ equal CID), and (2) distinct records get distinct CIDs.
   Property (2) is BLAKE3 collision resistance — a CRYPTOGRAPHIC assumption we do NOT
   and CANNOT prove here. We model it as an explicit INJECTIVITY hypothesis
   `hinj : ∀ a b, cidf a = cidf b → a = b`, carried on exactly the theorems that
   need it. This is the faithful "abstract non-collision over the finite model
   domain" the plan allows — a stand-in for the crypto, honestly flagged as an
   assumption, never claimed as a theorem. The DEFINITIONS never assume injectivity;
   only the security laws do.

   ## What `lake build` machine-checks (the store laws the Rust store relies on)

     same_record_same_cid / cid_injective    determinism + (assumed) non-collision
     distinct_records_distinct_cid            different bytes cannot alias (contrapos.)
     stage_preserves_record_and_cid           staging is PURE: (cidf r, r), no store
     committed_handle_resolves                 a committed CID resolves to its record
     rejected_candidate_not_published          staging alone publishes NOTHING
     published_handle_is_committed             every resolvable handle IS its own CID
     published_handle_has_matching_session     the store holds only session-scoped recs
     foreign_session_record_not_fetchable      a foreign-nonce record is not fetchable
     commit_batch_all_or_none                  reject ⇒ unchanged; accept ⇒ ALL installed
     failed_commit_preserves_store             a rejected batch leaves the store identical

   Theorems 5/6/7 are derived from a store INVARIANT `Valid` — every entry is keyed
   by its own content-CID and scoped to this session — proved established at the
   empty store and PRESERVED by `commitOne` / `commitBatch`. That is the clean way:
   the security properties are corollaries of a maintained invariant, not unproven
   assumptions.

   ## Deliberately NOT claimed here (future obligations)

   - BLAKE3 collision resistance / the canonical-encoding determinism — assumed
     (`hinj`), not proved; grounded by the Rust CID tests.
   - PUBLICATION ORDERING under concurrency (§2.12 / B6): that a stage→commit is
     atomic w.r.t. an interleaved external write, and that all-or-none holds under a
     concurrent committer, is a TLA+ obligation (`ContentSpillBatchCommit`) plus the
     Rust behavioral tests — this pure single-thread algebra does not model
     interleavings.

   No Mathlib; bare toolchain; fully machine-checked with no proof holes. -/

-- `Cid` is an abstract type carried through the whole development as a section
-- variable; `DecidableEq Cid` is needed by `commitOne`/`commitBatch` but not by the
-- pure CID/identity laws, so silence the (purely stylistic) unused-section-variable
-- linter rather than `omit` it from a dozen declarations.
set_option linter.unusedSectionVars false

namespace NewtPolicy.CompactionSpill

abbrev Schema := Nat
abbrev Nonce := Nat
/-- Provenance tag: 0 = ToolOutput, 1 = CompactionSpan. A `Nat` models the finite
    tag domain; bound into identity so a tool output and a compaction span with the
    same text never share a CID (via injectivity). -/
abbrev Prov := Nat
abbrev Text := Nat

/-- The versioned, session-scoped spill record whose CID IS the handle. Mirrors the
    Rust `SpillRecordV1 { schema, scope, provenance, redacted_text }`. Deterministic
    equality over the finite model domain. -/
structure Record where
  schema : Schema
  nonce : Nonce
  provenance : Prov
  text : Text
  deriving DecidableEq, Repr

/-- The content store: the committed map (`fetchFn`) plus this store's session
    `nonce` (the scope the store OWNS and stamps). Parameterised by the abstract CID
    type. -/
structure Store (Cid : Type) where
  fetchFn : Cid → Option Record
  nonce : Nonce

variable {Cid : Type} [DecidableEq Cid]

/-- Resolve a handle: `None` unless the CID is present in THIS store (membership is
    the read-authorization boundary, exactly as in the Rust `fetch`). -/
def Store.fetch (s : Store Cid) (c : Cid) : Option Record := s.fetchFn c

/-- The empty store for a session `n`: nothing committed. -/
def emptyStore (n : Nonce) : Store Cid := { fetchFn := fun _ => none, nonce := n }

/-- A record belongs to session store `s` iff its nonce matches the store's scope. -/
def sessionScoped (s : Store Cid) (r : Record) : Prop := r.nonce = s.nonce

/-- STAGE (PURE): return the content-CID and the record, touching no store and
    allocating nothing. Mirrors Rust `StagedSpill::from_record` — the CID is a pure
    function of the content. -/
def stage (cidf : Record → Cid) (r : Record) : Cid × Record := (cidf r, r)

/-- COMMIT ONE: install `r` under its content-CID `cidf r`. Idempotent for identical
    bytes (installing `cidf r ↦ r` over an existing `cidf r ↦ r` is a no-op). -/
def Store.commitOne (cidf : Record → Cid) (s : Store Cid) (r : Record) : Store Cid :=
  { s with fetchFn := fun c => if c = cidf r then some r else s.fetchFn c }

/-- `commitOne` leaves the session scope untouched. -/
theorem commitOne_nonce (cidf : Record → Cid) (s : Store Cid) (r : Record) :
    (Store.commitOne cidf s r).nonce = s.nonce := rfl

/-- The just-committed CID resolves to exactly its own record. -/
theorem commitOne_fetch_self (cidf : Record → Cid) (s : Store Cid) (r : Record) :
    (Store.commitOne cidf s r).fetch (cidf r) = some r := by
  show (if cidf r = cidf r then some r else s.fetch (cidf r)) = some r
  simp

/-- A commit of `r2` PRESERVES any already-resolved entry `cidf r ↦ r` — a later
    write cannot silently rebind a present handle to different bytes. Uses
    injectivity: if `cidf r2 = cidf r` then `r2 = r`, so the overwrite is with the
    SAME record. -/
theorem commitOne_preserves_resolved (cidf : Record → Cid)
    (hinj : ∀ a b, cidf a = cidf b → a = b)
    (s : Store Cid) (r r2 : Record) (h : s.fetch (cidf r) = some r) :
    (Store.commitOne cidf s r2).fetch (cidf r) = some r := by
  have hf : (Store.commitOne cidf s r2).fetch (cidf r)
          = (if cidf r = cidf r2 then some r2 else s.fetch (cidf r)) := rfl
  rw [hf]
  by_cases hc : cidf r = cidf r2
  · have hrr : r = r2 := hinj _ _ hc
    subst hrr
    simp
  · rw [if_neg hc]; exact h

/-- INSTALL a whole batch, left to right. Used only after the batch has been
    validated (see `commitBatch`); on its own it is the raw fold. -/
def Store.installAll (cidf : Record → Cid) : Store Cid → List Record → Store Cid
  | s, [] => s
  | s, a :: rest => Store.installAll cidf (Store.commitOne cidf s a) rest

/-- Installing more records preserves any already-resolved entry (fold of
    `commitOne_preserves_resolved`). -/
theorem installAll_preserves_resolved (cidf : Record → Cid)
    (hinj : ∀ a b, cidf a = cidf b → a = b) :
    ∀ (batch : List Record) (s : Store Cid) (r : Record),
      s.fetch (cidf r) = some r →
      (Store.installAll cidf s batch).fetch (cidf r) = some r
  | [], s, r, h => by simpa [Store.installAll] using h
  | a :: rest, s, r, h => by
      simp only [Store.installAll]
      exact installAll_preserves_resolved cidf hinj rest (Store.commitOne cidf s a) r
        (commitOne_preserves_resolved cidf hinj s r a h)

/-- EVERY member of an installed batch resolves to itself. The tricky case (the head
    surviving the rest) uses injectivity via `installAll_preserves_resolved`. -/
theorem installAll_fetch_of_mem (cidf : Record → Cid)
    (hinj : ∀ a b, cidf a = cidf b → a = b) :
    ∀ (batch : List Record) (s : Store Cid) (r : Record),
      r ∈ batch → (Store.installAll cidf s batch).fetch (cidf r) = some r
  | [], _, r, hmem => absurd hmem (by simp)
  | a :: rest, s, r, hmem => by
      simp only [Store.installAll]
      rcases List.mem_cons.mp hmem with h | h
      · subst h
        exact installAll_preserves_resolved cidf hinj rest (Store.commitOne cidf s r) r
          (commitOne_fetch_self cidf s r)
      · exact installAll_fetch_of_mem cidf hinj rest (Store.commitOne cidf s a) r h

/-- Installing preserves the session scope (the fold never touches `nonce`). -/
theorem installAll_nonce (cidf : Record → Cid) :
    ∀ (batch : List Record) (s : Store Cid),
      (Store.installAll cidf s batch).nonce = s.nonce
  | [], s => rfl
  | a :: rest, s => by
      simp only [Store.installAll]
      rw [installAll_nonce cidf rest (Store.commitOne cidf s a), commitOne_nonce]

/-! ### The batch admission predicate + `commitBatch` (all-or-none). -/

/-- A batch is admissible against `s` iff every record is (a) session-scoped and
    (b) content-consistent — its CID is either FRESH or already maps to that SAME
    record (idempotent dedup). A CID present with a DIFFERENT record is an integrity
    violation ⇒ inadmissible ⇒ the whole batch is rejected. Mirrors the Rust
    `commit_batch` prevalidation (integrity check) + the store-owned scope. -/
def Store.batchOk (cidf : Record → Cid) (s : Store Cid) (batch : List Record) : Prop :=
  ∀ r ∈ batch, r.nonce = s.nonce ∧ (s.fetch (cidf r) = none ∨ s.fetch (cidf r) = some r)

instance instDecidableBatchOk (cidf : Record → Cid) (s : Store Cid) (batch : List Record) :
    Decidable (Store.batchOk cidf s batch) := by
  unfold Store.batchOk
  exact List.decidableBAll _ batch

/-- COMMIT BATCH — ALL-OR-NONE. Prevalidate the WHOLE batch; if admissible install
    every record, else return `none` and leave the store untouched. Mirrors Rust
    `commit_batch` returning `Result<_, IntegrityViolation>`. -/
def Store.commitBatch (cidf : Record → Cid) (s : Store Cid) (batch : List Record) :
    Option (Store Cid) :=
  if Store.batchOk cidf s batch then some (Store.installAll cidf s batch) else none

/-- The effective store after a batch: unchanged (`s`) on rejection (mirrors Rust
    `&self` being untouched when `commit_batch` errs). -/
def Store.tryCommitBatch (cidf : Record → Cid) (s : Store Cid) (batch : List Record) :
    Store Cid :=
  (Store.commitBatch cidf s batch).getD s

/-- A successful batch was admissible and its result is exactly `installAll`. -/
theorem commitBatch_some (cidf : Record → Cid) {s s' : Store Cid} {batch : List Record}
    (h : Store.commitBatch cidf s batch = some s') :
    Store.batchOk cidf s batch ∧ s' = Store.installAll cidf s batch := by
  unfold Store.commitBatch at h
  by_cases hb : Store.batchOk cidf s batch
  · rw [if_pos hb] at h
    exact ⟨hb, (Option.some.inj h).symm⟩
  · rw [if_neg hb] at h
    simp at h

/-! ### The store INVARIANT `Valid` and its preservation. -/

/-- The maintained invariant: every resolvable handle equals the content-CID of the
    record it resolves to, AND that record is scoped to this session. Established at
    the empty store, preserved by every commit. -/
def Valid (cidf : Record → Cid) (s : Store Cid) : Prop :=
  ∀ c r, s.fetch c = some r → c = cidf r ∧ r.nonce = s.nonce

/-- The empty store is `Valid` (vacuously — it resolves nothing). -/
theorem valid_empty (cidf : Record → Cid) (n : Nonce) : Valid cidf (emptyStore n) := by
  intro c r h
  simp [emptyStore, Store.fetch] at h

/-- `commitOne` PRESERVES `Valid` for a session-scoped record. -/
theorem commitOne_preserves_valid (cidf : Record → Cid) (s : Store Cid) (r : Record)
    (hv : Valid cidf s) (hscope : r.nonce = s.nonce) :
    Valid cidf (Store.commitOne cidf s r) := by
  intro c r' h
  have hf : (Store.commitOne cidf s r).fetch c
          = (if c = cidf r then some r else s.fetch c) := rfl
  rw [hf] at h
  rw [commitOne_nonce]
  by_cases hc : c = cidf r
  · rw [if_pos hc] at h
    have hrr : r = r' := Option.some.inj h
    subst hrr
    exact ⟨hc, hscope⟩
  · rw [if_neg hc] at h
    exact hv c r' h

/-- `installAll` PRESERVES `Valid` when every record is session-scoped. -/
theorem installAll_preserves_valid (cidf : Record → Cid) :
    ∀ (batch : List Record) (s : Store Cid),
      Valid cidf s → (∀ r ∈ batch, r.nonce = s.nonce) →
      Valid cidf (Store.installAll cidf s batch)
  | [], s, hv, _ => by simpa [Store.installAll] using hv
  | a :: rest, s, hv, hsc => by
      simp only [Store.installAll]
      have ha : a.nonce = s.nonce := hsc a List.mem_cons_self
      have hv1 : Valid cidf (Store.commitOne cidf s a) :=
        commitOne_preserves_valid cidf s a hv ha
      have hsc1 : ∀ r ∈ rest, r.nonce = (Store.commitOne cidf s a).nonce := by
        intro r hr
        rw [commitOne_nonce]
        exact hsc r (List.mem_cons_of_mem a hr)
      exact installAll_preserves_valid cidf rest (Store.commitOne cidf s a) hv1 hsc1

/-- `commitBatch` PRESERVES `Valid` (admission forces session scope). -/
theorem commitBatch_preserves_valid (cidf : Record → Cid) {s s' : Store Cid}
    {batch : List Record} (hv : Valid cidf s)
    (h : Store.commitBatch cidf s batch = some s') : Valid cidf s' := by
  obtain ⟨hok, hs'⟩ := commitBatch_some cidf h
  subst hs'
  exact installAll_preserves_valid cidf batch s hv (fun r hr => (hok r hr).1)

/-! ### The nine machine-checked laws. -/

/-- (1a) Determinism: equal records ⇒ equal CID. -/
theorem same_record_same_cid (cidf : Record → Cid) {r1 r2 : Record} (h : r1 = r2) :
    cidf r1 = cidf r2 := by rw [h]

/-- (1b) Injectivity (the ASSUMED non-collision, stated as a hypothesis — NOT a
    BLAKE3 proof): equal CIDs ⇒ equal records. -/
theorem cid_injective (cidf : Record → Cid) (hinj : ∀ a b, cidf a = cidf b → a = b)
    {r1 r2 : Record} (h : cidf r1 = cidf r2) : r1 = r2 := hinj r1 r2 h

/-- (1c) Contrapositive: distinct records cannot alias (different bytes ⇒ different
    CID). Grounds the Rust `different_content` / `different_provenance` /
    `different_session_nonce` yield-different-CID tests. -/
theorem distinct_records_distinct_cid (cidf : Record → Cid)
    (hinj : ∀ a b, cidf a = cidf b → a = b) {r1 r2 : Record} (h : r1 ≠ r2) :
    cidf r1 ≠ cidf r2 := fun hcid => h (hinj r1 r2 hcid)

/-- (2) Staging is PURE: returns exactly the content-CID and the record, no store. -/
theorem stage_preserves_record_and_cid (cidf : Record → Cid) (r : Record) :
    stage cidf r = (cidf r, r) := rfl

/-- (3) A committed handle resolves to its record — for a single commit and for
    every member of a successful batch. -/
theorem committed_handle_resolves (cidf : Record → Cid) (s : Store Cid) (r : Record) :
    (Store.commitOne cidf s r).fetch (cidf r) = some r :=
  commitOne_fetch_self cidf s r

/-- (4) A merely-STAGED (not-committed) candidate publishes NOTHING: staging returns
    a handle but installs no entry, so on a store without it the content-CID still
    resolves to `none`. -/
theorem rejected_candidate_not_published (cidf : Record → Cid) (s : Store Cid) (r : Record)
    (hfresh : s.fetch (cidf r) = none) :
    (stage cidf r).1 = cidf r ∧ s.fetch (stage cidf r).1 = none := by
  refine ⟨rfl, ?_⟩
  show s.fetch (cidf r) = none
  exact hfresh

/-- (5) Every resolvable handle IS the content-CID of the record it resolves to
    (from `Valid`). A handle is present only because it was committed under its own
    CID. -/
theorem published_handle_is_committed (cidf : Record → Cid) (s : Store Cid) (c : Cid)
    (r : Record) (hv : Valid cidf s) (h : s.fetch c = some r) : c = cidf r :=
  (hv c r h).1

/-- (6) A store holds only records scoped to ITS session (from `Valid`). -/
theorem published_handle_has_matching_session (cidf : Record → Cid) (s : Store Cid)
    (c : Cid) (r : Record) (hv : Valid cidf s) (h : s.fetch c = some r) :
    sessionScoped s r :=
  (hv c r h).2

/-- (7) A record built under a DIFFERENT nonce is NOT fetchable here — its CID was
    never committed to this session's store. Uses `Valid` + injectivity. -/
theorem foreign_session_record_not_fetchable (cidf : Record → Cid)
    (hinj : ∀ a b, cidf a = cidf b → a = b) (s : Store Cid) (r : Record)
    (hv : Valid cidf s) (hforeign : r.nonce ≠ s.nonce) : s.fetch (cidf r) = none := by
  cases h : s.fetch (cidf r) with
  | none => rfl
  | some r' =>
      exfalso
      have hc : cidf r = cidf r' := (hv (cidf r) r' h).1
      have hn : r'.nonce = s.nonce := (hv (cidf r) r' h).2
      have hrr : r = r' := hinj _ _ hc
      subst hrr
      exact hforeign hn

/-- (8) ALL-OR-NONE. A rejected batch leaves every fetch unchanged; an accepted
    batch installs EVERY record. -/
theorem commit_batch_all_or_none (cidf : Record → Cid)
    (hinj : ∀ a b, cidf a = cidf b → a = b) (s : Store Cid) (batch : List Record) :
    (Store.commitBatch cidf s batch = none →
        ∀ c, (Store.tryCommitBatch cidf s batch).fetch c = s.fetch c) ∧
    (∀ s', Store.commitBatch cidf s batch = some s' →
        ∀ r ∈ batch, s'.fetch (cidf r) = some r) := by
  refine ⟨?_, ?_⟩
  · intro hnone c
    have : Store.tryCommitBatch cidf s batch = s := by
      unfold Store.tryCommitBatch; rw [hnone]; rfl
    rw [this]
  · intro s' hsome r hr
    obtain ⟨_, hs'⟩ := commitBatch_some cidf hsome
    subst hs'
    exact installAll_fetch_of_mem cidf hinj batch s r hr

/-- (9) A FAILED batch leaves the store byte-identical — no partial install. -/
theorem failed_commit_preserves_store (cidf : Record → Cid) {s : Store Cid}
    {batch : List Record} (h : Store.commitBatch cidf s batch = none) :
    Store.tryCommitBatch cidf s batch = s := by
  unfold Store.tryCommitBatch; rw [h]; rfl

/-! ### Non-vacuity: a concrete instantiation (`Cid := Record`, `cidf := id`).

    `id` is the honest, `rfl`-injective model injection (see the header). These
    exhibit the laws firing on concrete data — a `Valid` non-empty store, a foreign
    fetch that is `none`, and both branches of all-or-none. -/

def demoNonce : Nonce := 42
def recA : Record := { schema := 1, nonce := demoNonce, provenance := 0, text := 100 }
def recB : Record := { schema := 1, nonce := demoNonce, provenance := 1, text := 200 }
/-- A FOREIGN record: a different session nonce. -/
def recForeign : Record := { schema := 1, nonce := 99, provenance := 0, text := 100 }

/-- The model injection is injective by `rfl` — honest, not a crypto claim. -/
theorem id_injective : ∀ a b : Record, id a = id b → a = b := fun _ _ h => h

/-- Distinct records (here: different provenance / nonce) get distinct CIDs. -/
example : (id recA) ≠ (id recForeign) :=
  distinct_records_distinct_cid id id_injective (by decide)

/-- A store holding `recA`, built by committing into the empty store. -/
def demoStore : Store Record := Store.commitOne id (emptyStore demoNonce) recA

/-- The committed handle resolves. -/
example : demoStore.fetch (id recA) = some recA :=
  committed_handle_resolves id (emptyStore demoNonce) recA

/-- `demoStore` is `Valid` (empty store is valid; a session-scoped commit preserves it). -/
theorem demoStore_valid : Valid id demoStore :=
  commitOne_preserves_valid id (emptyStore demoNonce) recA (valid_empty id demoNonce) rfl

/-- Law (6) firing concretely: the resolvable record shares the session scope. -/
example : sessionScoped demoStore recA :=
  published_handle_has_matching_session id demoStore (id recA) recA demoStore_valid
    (committed_handle_resolves id (emptyStore demoNonce) recA)

/-- Law (7) firing concretely: a foreign-nonce record is not fetchable. -/
example : demoStore.fetch (id recForeign) = none :=
  foreign_session_record_not_fetchable id id_injective demoStore recForeign demoStore_valid
    (by decide)

/-- A batch containing a foreign-nonce record is REJECTED (all-or-none). -/
example : Store.commitBatch id demoStore [recForeign] = none := by
  have hnot : ¬ Store.batchOk id demoStore [recForeign] := by
    intro hok
    exact absurd (hok recForeign List.mem_cons_self).1 (by decide)
  unfold Store.commitBatch
  exact if_neg hnot

/-- A clean batch is ACCEPTED and installs every member. -/
theorem demo_commitBatch_ok :
    Store.commitBatch id (emptyStore demoNonce) [recA, recB]
      = some (Store.installAll id (emptyStore demoNonce) [recA, recB]) := by
  have hok : Store.batchOk id (emptyStore demoNonce) [recA, recB] := by
    intro r hr
    refine ⟨?_, Or.inl rfl⟩
    rcases List.mem_cons.mp hr with h | h
    · subst h; rfl
    · rcases List.mem_cons.mp h with h | h
      · subst h; rfl
      · exact absurd h (by simp)
  unfold Store.commitBatch
  exact if_pos hok

/-- Law (8) accept-branch firing concretely: a batched member resolves. -/
example : (Store.installAll id (emptyStore demoNonce) [recA, recB]).fetch (id recB) = some recB :=
  installAll_fetch_of_mem id id_injective [recA, recB] (emptyStore demoNonce) recB
    (List.mem_cons_of_mem recA List.mem_cons_self)

end NewtPolicy.CompactionSpill
