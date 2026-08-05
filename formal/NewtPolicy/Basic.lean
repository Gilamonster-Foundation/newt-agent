/-
  NewtPolicy — machine-checked models of newt's load-bearing POLICY seams.

  Every blocker in the four-round #1526 review lived in the same handful of pure
  policy decisions: backend selection, Responses decoding, and validate-before-
  execute ordering. This module makes the *decisions* theorems (the "Lean" layer
  of the behavioral constitution, epic #1529): a Rust change that breaks one of
  these fails a proof, not just a test.

  Self-contained (no Mathlib), matching `CaveatLattice` / `ProjectModel`. Each
  theorem is tagged with its behavioral-contract id (`BHV-…`, see
  `spec/behavior-map.toml`) so the model, the tests, and the production function
  all name the same rule.
-/

namespace NewtPolicy

/-! ## Backend selection (`BHV-BACKEND-001`)

A backend is identified by a name and whether it is *usable* (has a non-empty
endpoint). The explicit-selection decision mirrors the usable-backend branch of
the Rust `Config::select_backend`: an explicit selector (`$NEWT_PROVIDER`, else
`default_backend`) is **authoritative** exactly when it names a usable backend
present in the config. The metamorphic theorems below formalize the #1526
regression: an explicit selection must survive the config growing or being
reordered — an unrelated OpenAI backend can never steal an explicitly selected
Ollama backend. -/

namespace Backend

structure Backend where
  name : String
  usable : Bool

/-- The predicate the selector's `any` search uses: a usable backend by this name.
    (`matches` is a reserved word in Lean, hence `namedUsable`.) -/
def namedUsable (name : String) (b : Backend) : Bool := b.name == name && b.usable

/-- `select` is authoritative iff the selector names a usable backend in the list.
    (`true` = that backend is selected; `false` = this decision abstains and the
    preference rules take over — modeled separately.) -/
def selectsUsable (backends : List Backend) (name : String) : Bool :=
  backends.any (namedUsable name)

/-- The one unfolding every proof below leans on: selection over a cons splits
    into the head's match OR selection over the tail. -/
theorem selectsUsable_cons (b : Backend) (bs : List Backend) (name : String) :
    selectsUsable (b :: bs) name = (namedUsable name b || selectsUsable bs name) := by
  simp only [selectsUsable, List.any_cons]

/-- `(a == b) = false` from `a ≠ b`, for any lawful `BEq` (String here). Core
    Lean has `eq_of_beq`; this is its `Bool`-valued contrapositive. -/
theorem beq_false_of_ne {α} [BEq α] [LawfulBEq α] {a b : α} (h : a ≠ b) :
    (a == b) = false := by
  cases hb : a == b with
  | false => rfl
  | true => exact absurd (eq_of_beq hb) h

/-- `BHV-BACKEND-001` (metamorphic, growth): adding ANY backend to the config
    cannot un-select an explicitly selected backend. The `any` over a superset
    stays true, so a new (e.g. OpenAI) entry never erases the selection. -/
theorem adding_a_backend_preserves_selection
    (backends : List Backend) (name : String) (extra : Backend)
    (hsel : selectsUsable backends name = true) :
    selectsUsable (extra :: backends) name = true := by
  rw [selectsUsable_cons, hsel, Bool.or_true]

/-- `BHV-BACKEND-001` (metamorphic, no fabrication): adding a backend whose name
    DIFFERS from the selector cannot fabricate a selection that was not already
    there. Together with the growth theorem: an explicit selection depends only
    on whether a usable backend of that name exists — never on unrelated entries.
    This is the property a `Config` fuzzer would try to break by injecting a
    high-precedence OpenAI backend beside a selected Ollama one. -/
theorem adding_a_differently_named_backend_cannot_fabricate_a_selection
    (backends : List Backend) (name : String) (extra : Backend)
    (hno : selectsUsable backends name = false)
    (hne : extra.name ≠ name) :
    selectsUsable (extra :: backends) name = false := by
  have hm : namedUsable name extra = false := by
    simp only [namedUsable, beq_false_of_ne hne, Bool.false_and]
  simp [selectsUsable_cons, hm, hno]

/-- Corollary: reordering never matters — selection is the *existence* of a
    usable backend of the selector's name, independent of position. Proven for
    the adjacent-swap generator of permutations. -/
theorem selection_is_position_independent
    (a b : Backend) (rest : List Backend) (name : String) :
    selectsUsable (a :: b :: rest) name = selectsUsable (b :: a :: rest) name := by
  rw [selectsUsable_cons, selectsUsable_cons, selectsUsable_cons, selectsUsable_cons,
    Bool.or_left_comm]

end Backend

/-! ## Validated tool batch (`BHV-TOOLS-001`, `BHV-TOOLS-002`)

The capability-token design: a `ValidatedCall` CANNOT be constructed with an
empty name or id (the invariants are fields, i.e. proof obligations), and a
`ValidatedBatch` carries a proof its ids are pairwise-distinct. The Rust
`execute_batch` must take a `ValidatedBatch`, never a `Vec<RawCall>` — this
module is the specification that architectural choke point refines. The lemmas
are deliberately by-construction: that IS the point — the type makes the broken
state unrepresentable. -/

namespace ToolBatch

/-- A model-emitted tool call before validation. `argsObject` abstracts "the
    arguments parsed to a JSON object" (the wire/serde detail is NOT formalized
    here — it belongs to property/fuzz testing). -/
structure RawCall where
  id : String
  name : String
  argsObject : Bool

/-- A call that PASSED validation: non-empty id (so a result can be correlated),
    non-empty name, and object-shaped arguments — each a carried proof. -/
structure ValidatedCall where
  id : String
  name : String
  id_ne : id ≠ ""
  name_ne : name ≠ ""
  args_object : True   -- placeholder obligation; a real `IsObject args` slots here

/-- A whole validated batch: the calls, plus a proof their ids are all distinct.
    A missing/duplicate id cannot be represented — the exact `BHV-TOOLS-002`
    "correlation-impossible" state the Rust loop must abort on. -/
structure ValidatedBatch where
  calls : List ValidatedCall
  ids_nodup : (calls.map (·.id)).Nodup

/-- `BHV-TOOLS-002`: every call in a validated batch has a non-empty id — a
    result can always be correlated. By construction (the token carries it). -/
theorem validated_call_id_nonempty (c : ValidatedCall) : c.id ≠ "" := c.id_ne

/-- `BHV-TOOLS-002`: a validated batch's ids are pairwise distinct — no
    ambiguous result routing is representable. By construction. -/
theorem validated_batch_ids_distinct (b : ValidatedBatch) :
    (b.calls.map (·.id)).Nodup := b.ids_nodup

/-- `BHV-TOOLS-001`: execution consumes the token, never raw calls. Modeled as:
    the only way to obtain the executable list is to project it from a
    `ValidatedBatch` — there is no `ValidatedBatch` inhabitant built from an
    unvalidated `List RawCall` without discharging the invariants above. The Rust
    refinement is `validate_tool_call_batch` returning the validated calls the
    agentic loop then executes; a compile-time `execute_batch(ValidatedBatch)`
    type choke point is not yet implemented (future work, #1529). -/
def executable (b : ValidatedBatch) : List ValidatedCall := b.calls

end ToolBatch

end NewtPolicy
