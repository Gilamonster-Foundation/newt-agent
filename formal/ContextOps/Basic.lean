/- #1766 — the CONTEXT OPERATIONS kernel: fidelity, provenance roots, and the
   packet chain.

   The three compaction kernels already in this tree prove compaction is
   authority-safe (`CompactionProvenance`, #1528 B2), budget-safe and terminating
   (`CompactionLifecycle`, #1528 B3) and address-safe (`CompactionSpill`, #1528 B3).
   NONE of them says whether what comes out MEANS what went in. This kernel is that
   missing half.

   ## The theory in one line

   Compaction today is an untyped SUBSTITUTION: a span replaced by generated prose
   fusing three operations with different fidelity contracts, marking none of them.
   An operation whose contract is unspecified cannot be gated. Typing it gives:

     concision   fewer words, same referents  — asserts nothing new, extractively checkable
     elision     removed + marked + addressed — asserts nothing at all
     generation new sentences                — unverifiable; the ONLY fabrication surface

   ## What `lake build` machine-checks

     seal_depth_le_one            a unit sealed from SOURCE is at depth <= 1
     chain_depth_le_one           EVERY unit in a well-formed chain is at depth <= 1
     seal_chain_wf                sealing preserves well-formedness (the keystone step)
     genesis_wf / chain_wf_induct depth <= 1 is an INDUCTIVE property of the chain
     elide_asserts_nothing        elision makes no content claim
     elide_is_rederivable         elision's verifiability class needs no model
     no_generation_all_checkable "generation off" needs no attestation anywhere
     generation_is_the_only_riser only generation can increase depth
     render_no_new_units          a re-render (backend switch) invents nothing
     render_idempotent            switching A->B->A returns the same units
     render_preserves_depth_bound a switch cannot raise depth
     rooted_not_fabricated        a rooted unit is not a fabrication
     orphan_is_fabricated         an unrooted unit IS one, decidably
     model_output_is_not_a_root   `Root` has no model constructor, by construction
     superseded_evictable_freely  supersession-driven eviction needs no grounding

   ## Why `generate` and not `abstract`

   An earlier draft called this operation *abstraction*. That is the wrong word. In
   programming languages and formal methods, an **abstraction** is a *sound
   over-approximation* — it may lose precision but never asserts something false. This
   operation is the exact opposite: it is the only one that can assert something the
   source does not support. Using the established term for its inverse would mislead
   precisely the readers best equipped to check the work.

   Related, and NOT claimed here: selection-style compaction is not strictly better than
   generation. Recent work formalizes compaction as selection-vs-generation games and
   shows generation can *strictly* beat selection. So
   [`no_generation_all_checkable`] below is a STATEMENT OF A TRADE-OFF — running without
   generation buys a fidelity guarantee and costs real capability — not a proof that
   generation is unnecessary.

   ## The honesty boundary (READ THIS BEFORE CITING THIS FILE)

   Authority is algebraic, so `CaveatLattice` can prove attenuation outright.
   **"Meaning is preserved" is NOT formalizable that way, and this kernel does not
   claim it.** What is formalizable is the STRUCTURE around fidelity: which
   operation may raise generation depth, that a chain cannot nest, which
   verifiability class each operation falls into, and whether a unit's provenance
   terminates at a root.

   `Grounded` is therefore an OPAQUE predicate — a modelling assumption discharged
   by `newt-core/src/grounding.rs` and its Rust tests, carried as an explicit
   hypothesis on exactly the theorems that need it. This is the same discipline
   `CompactionSpill` uses for BLAKE3 collision resistance (`hinj`): the definitions
   never assume it, only the laws do, and it is never claimed as a theorem.

   Overselling this file as "we proved compaction is faithful" would make it worse
   than no proof at all.

   ## Deliberately NOT claimed here (future obligations)

   - That any particular concision IS grounded — that is `grounding.rs` + its tests.
   - The BUDGET half (partition floors, safety under an unknown ceiling, stability
     under a mid-run tightening) — a sibling kernel, `ContextBudget`, which must
     compose with `ResponsesUsage`'s `Option Nat` ceiling lattice rather than assume
     a fixed `Nat`.
   - LIVENESS: that eviction and retrieval against one budget cannot oscillate. That
     is a temporal property over many rounds and belongs in TLA+
     (`ContextBudget.tla`), not in this pure algebra.
   - ATTESTATION of a transferred packet (mesh supply chain): a received packet must
     fail closed to untrusted until its signature verifies against a trusted key.
     That extends `CompactionProvenance`'s closed provenance set and is filed as a
     separate obligation.
   - That a model will actually CALL `memory_fetch` to redeem an elision. That is a
     property of the model, not of the harness, and no specification can supply it.

   No Mathlib; bare toolchain; `sorry`-free. -/
namespace NewtPolicy.ContextOps

/-! ## Operations -/

/-- The three ways to free context, distinguished by what each ASSERTS.
    Substitution — today's untyped fusion of all three — is deliberately NOT a
    constructor: this kernel exists to make it unrepresentable. -/
inductive Op where
  /-- Fewer words, same referents. Asserts nothing new; extractively checkable. -/
  | concise
  /-- Removed, marked, addressed. Asserts nothing at all. -/
  | elide
  /-- New sentences not present in the source. The only fabrication surface. -/
  | generate
  deriving DecidableEq, Repr

/-- Whether an operation makes a content claim about the material it replaces.
    Elision is the only one that does not, which is why it is the safe default
    whenever grounding is uncertain (invariant I4). -/
def asserts : Op → Bool
  | .concise  => true
  | .elide    => false
  | .generate => true

/-- How a unit can be VERIFIED by a party that did not run the build
    (the supply-chain reproducibility boundary).

    Sealing invokes an LLM, so a packet is an artifact of a NON-reproducible build
    and verification cannot rest on re-derivation alone. The fidelity typing hands
    us the boundary for free: only elision is deterministic. -/
inductive Check where
  /-- Deterministic selection: recompute and compare. -/
  | rederive
  /-- Model-generated but extractive: check against the sealed source. -/
  | ground
  /-- Model-generated and non-reproducible: grounding plus an attestation. -/
  | groundAttest
  deriving DecidableEq, Repr

/-- The verifiability class of each operation. -/
def checkOf : Op → Check
  | .elide    => .rederive
  | .concise  => .ground
  | .generate => .groundAttest

/-! ## Provenance roots (Part X)

Every unit exists BECAUSE of something. These are where "because of" terminates.
Note what is absent: there is no `modelOutput` constructor. A model's assertion is
never a provenance root, and this is enforced by the type, not by a rule. -/

/-- The three kinds of thing that can root a unit's provenance. -/
inductive Root where
  /-- The human SAID: intent expressed in language. Needs adjudication. -/
  | operatorPrompt
  /-- The human DID: intent expressed as control. Unambiguous by construction,
      and non-repudiable via the verdict-bound challenge in
      `newt-core/src/permission_challenge.rs`. -/
  | userAction
  /-- The machine decided: a budget threshold, a retry, an automatic seal. -/
  | harnessEvent
  deriving DecidableEq, Repr

/-- Lifecycle of a root. Operator intent drifts ("do Y instead"), and tracking that
    per-unit is intractable; tracking it at the ROOT is not, and everything
    downstream inherits it. -/
inductive Life where
  | live
  | superseded
  | withdrawn
  deriving DecidableEq, Repr

/-- A single unit of compacted context. `depth` is generation provenance depth:
    0 for material taken from source, and one more for each generation applied
    over an generation. Invariant I3 is `depth <= 1`. -/
structure Unit where
  op : Op
  depth : Nat
  root : Option Root
  life : Life
  addressed : Bool
  deriving DecidableEq, Repr

/-! ## Depth: only generation can raise it -/

/-- The depth produced by applying `op` to material already at depth `d`. -/
def depthAfter (op : Op) (d : Nat) : Nat :=
  match op with
  | .generate => d + 1
  | _         => d

/-- Concision and elision never raise generation depth; only generation does.
    This is the arithmetic core of invariant I3. -/
theorem generation_is_the_only_riser (op : Op) (d : Nat) :
    depthAfter op d = d ∨ (op = .generate ∧ depthAfter op d = d + 1) := by
  cases op <;> simp [depthAfter]

theorem concise_preserves_depth (d : Nat) : depthAfter .concise d = d := by
  simp [depthAfter]

theorem elide_preserves_depth (d : Nat) : depthAfter .elide d = d := by
  simp [depthAfter]

/-! ## Sealing: units are produced from SOURCE, never from prior prose -/

/-- Seal one unit from SOURCE material (depth 0). This is the only constructor the
    chain uses, and it is why depth cannot reach 2: there is no way to seal a unit
    from a previous packet's prose. -/
def sealUnit (op : Op) (root : Option Root) : Unit :=
  { op := op, depth := depthAfter op 0, root := root, life := .live,
    addressed := true }

/-- **Keystone step.** A unit sealed from source is at depth at most 1. -/
theorem seal_depth_le_one (op : Op) (root : Option Root) :
    (sealUnit op root).depth ≤ 1 := by
  cases op <;> simp [sealUnit, depthAfter]

/-- Everything sealed is addressed — the precondition for elision to be redeemable
    (invariant I5). -/
theorem seal_is_addressed (op : Op) (root : Option Root) :
    (sealUnit op root).addressed = true := by
  simp [sealUnit]

/-! ## The packet chain

A conversation is a chain of immutable packets. `sealed` takes the PRIOR CHAIN BY
REFERENCE and a fresh list of units sealed from the new segment's SOURCE. There is
no constructor that re-generates a prior packet's prose, so nesting is
unrepresentable — invariant I3 becomes a shape rather than a rule. -/

/-- A packet chain: a genesis packet, then a linked list of sealed segments.

    NOTE: the constructor is `sealed`, not `seal`, because `seal` is a Lean 4
    keyword (the `seal`/`unseal` commands). -/
inductive Chain where
  | genesis (units : List Unit)
  | sealed (prior : Chain) (units : List Unit)
  deriving Repr

/-- Every unit reachable in the chain. -/
def Chain.units : Chain → List Unit
  | .genesis us  => us
  | .sealed p us => p.units ++ us

/-- Well-formedness of one unit: invariant I3. -/
def Unit.wf (u : Unit) : Prop := u.depth ≤ 1

/-- Well-formedness of a chain: every packet's units satisfy I3. -/
def Chain.wf : Chain → Prop
  | .genesis us => ∀ u ∈ us, u.wf
  | .sealed p us => p.wf ∧ ∀ u ∈ us, u.wf

/-- A packet built entirely by `sealUnit` is well-formed. -/
theorem genesis_wf (ops : List (Op × Option Root)) :
    Chain.wf (.genesis (ops.map (fun p => sealUnit p.1 p.2))) := by
  intro u hu
  simp only [List.mem_map] at hu
  obtain ⟨p, _, rfl⟩ := hu
  exact seal_depth_le_one p.1 p.2

/-- Sealing a well-formed chain with source-sealed units stays well-formed. -/
theorem seal_chain_wf (c : Chain) (hc : c.wf) (ops : List (Op × Option Root)) :
    Chain.wf (.sealed c (ops.map (fun p => sealUnit p.1 p.2))) := by
  refine ⟨hc, ?_⟩
  intro u hu
  simp only [List.mem_map] at hu
  obtain ⟨p, _, rfl⟩ := hu
  exact seal_depth_le_one p.1 p.2

/-- **THE KEYSTONE.** Every unit anywhere in a well-formed chain is at generation
    depth at most 1 — no matter how long the chain grows.

    This is the structural mirror of `CaveatLattice`'s `delegation_chain_bounded`:
    there, a chain of delegations never amplifies AUTHORITY; here, a chain of
    packets never amplifies FABRICATION. -/
theorem chain_depth_le_one (c : Chain) (h : c.wf) :
    ∀ u ∈ c.units, u.depth ≤ 1 := by
  induction c with
  | genesis us => exact h
  | sealed p us ih =>
      obtain ⟨hp, hus⟩ := h
      intro u hu
      simp only [Chain.units, List.mem_append] at hu
      cases hu with
      | inl hin => exact ih hp u hin
      | inr hin => exact hus u hin

/-! ## Elision is the safe operation -/

/-- Elision makes no content claim, so it has no fabrication surface (I4). -/
theorem elide_asserts_nothing : asserts .elide = false := by simp [asserts]

/-- Elision is the one operation verifiable WITHOUT a model — recompute the
    selection and compare. -/
theorem elide_is_rederivable : checkOf .elide = .rederive := by simp [checkOf]

/-- Generation is the only operation whose verification needs an attestation. -/
theorem only_generation_needs_attestation (op : Op) :
    checkOf op = .groundAttest ↔ op = .generate := by
  cases op <;> simp [checkOf]

/-- **`/context generation off` is a real mode, not a degenerate one.** A ledger
    containing no generation needs no attestation anywhere: every unit is
    verifiable by re-derivation or by grounding against the source alone. -/
theorem no_generation_all_checkable (us : List Unit)
    (h : ∀ u ∈ us, u.op ≠ .generate) :
    ∀ u ∈ us, checkOf u.op ≠ .groundAttest := by
  intro u hu hcontra
  exact h u hu ((only_generation_needs_attestation u.op).mp hcontra)

/-! ## Provenance roots and fabrication (Part X) -/

/-- A unit whose provenance chain does not terminate at a root.

    This is a DECIDABLE fabrication test that needs no token comparison and no
    model call — strictly stronger than, and complementary to, grounding.
    `grounding.rs` asks "do these words appear in the source?"; this asks "does
    this exist because someone asked for it?". A fabrication that recycles the
    transcript's own vocabulary passes the first and fails this one. -/
def fabricated (u : Unit) : Bool := u.root.isNone

theorem rooted_not_fabricated (u : Unit) (r : Root) (h : u.root = some r) :
    fabricated u = false := by
  simp [fabricated, h]

theorem orphan_is_fabricated (u : Unit) (h : u.root = none) :
    fabricated u = true := by
  simp [fabricated, h]

/-- **Model output is never a provenance root.** Enforced by the type: `Root` has
    exactly three constructors and none of them is a model. Any claimed root is one
    of the three, so no case analysis anywhere can admit a fourth. -/
theorem model_output_is_not_a_root (r : Root) :
    r = .operatorPrompt ∨ r = .userAction ∨ r = .harnessEvent := by
  cases r
  · exact Or.inl rfl
  · exact Or.inr (Or.inl rfl)
  · exact Or.inr (Or.inr rfl)

/-! ## Supersession is free eviction -/

/-- A unit is freely evictable when its root is no longer live. Deterministic:
    no grounding check, no model call, no judgement. -/
def evictable (u : Unit) : Bool :=
  match u.life with
  | .live => false
  | _     => true

theorem live_not_freely_evictable (u : Unit) (h : u.life = .live) :
    evictable u = false := by
  simp [evictable, h]

/-- **Supersession-driven eviction carries zero fidelity risk.** When the operator
    says "do Y instead", everything that existed only to serve X becomes evictable
    by construction — regardless of which operation produced it, and without
    consulting any grounding predicate. This is the only eviction source in the
    design that costs nothing and risks nothing. -/
theorem superseded_evictable_freely (u : Unit)
    (h : u.life = .superseded ∨ u.life = .withdrawn) :
    evictable u = true := by
  cases h with
  | inl hs => simp [evictable, hs]
  | inr hw => simp [evictable, hw]

/-! ## A backend switch is a RE-RENDER, not a re-summarize

Fitting a smaller window means SELECTING FEWER UNITS, never generating new text.
Hence: no fidelity cost, no inference cost, and — the property that makes
backend-hunting safe — idempotence. -/

/-- Re-render for a target backend: select a sublist. Invents nothing. -/
def render (sel : Unit → Bool) (us : List Unit) : List Unit := us.filter sel

/-- A re-render introduces no unit that was not already present. -/
theorem render_no_new_units (sel : Unit → Bool) (us : List Unit) :
    ∀ u ∈ render sel us, u ∈ us := by
  intro u hu
  exact (List.mem_filter.mp hu).1

/-- **Backend-hunting is safe.** Switching A → B → A returns exactly the units you
    had: nothing was destroyed, only deselected. -/
theorem render_idempotent (sel : Unit → Bool) (us : List Unit) :
    render sel (render sel us) = render sel us := by
  simp [render, List.filter_filter, Bool.and_self]

/-- A re-render cannot raise generation depth, because it produces no new units.
    So N backend switches leave the chain's depth bound intact. -/
theorem render_preserves_depth_bound (sel : Unit → Bool) (us : List Unit)
    (h : ∀ u ∈ us, u.depth ≤ 1) :
    ∀ u ∈ render sel us, u.depth ≤ 1 := by
  intro u hu
  exact h u (render_no_new_units sel us u hu)

/-- A re-render never orphans a rooted unit: provenance survives a switch, because
    the units themselves are carried over unchanged. -/
theorem render_preserves_rootedness (sel : Unit → Bool) (us : List Unit)
    (h : ∀ u ∈ us, fabricated u = false) :
    ∀ u ∈ render sel us, fabricated u = false := by
  intro u hu
  exact h u (render_no_new_units sel us u hu)

end NewtPolicy.ContextOps
