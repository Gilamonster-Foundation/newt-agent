/- #1528 B4 — the accepted-round USAGE-OBSERVATION policy kernel.

   A pure model of the budget-learning half of the agentic loop's per-round
   capability observations (Phase 20 §2.2, generalized in #1528 B4). Mirrors the
   Rust design in `newt-core/src/agentic/mod.rs` (the two chat dispatch loops) +
   `newt-core/src/agentic/send_budget.rs`:

   * A `BudgetState` carries a `hardCeiling : Option Nat` (the monotone input
     ceiling — `effective_input_ceiling` / `learned_hard_ceiling`; `none` = an
     unknown/unbounded window) and a `softBudget : Nat` (the `send_budget` a
     success may raise).
   * `observe` processes ONE provider response. It emits an `Accepted` observation
     — and raises the soft budget — ONLY for a response the classifier
     `isAccepted` accepts (completed usable text OR a FULLY-VALIDATED tool-call
     batch). The raise CLAMPS to the hard ceiling (`capTo`, mirroring
     `capped_accepted_prompt_tokens`) and only ever moves UP; it NEVER touches the
     hard ceiling.
   * `tighten` models a context-window 400: it may only tighten the hard ceiling
     (monotone, `newHardCeiling` via `Nat.min`), never raise it.

   ## What `lake build` machine-checks (the budget-learning laws the loops rely on)

     hard_ceiling_never_increases                        observe/tighten leave hard ≤ its prior value
     accepted_observation_cannot_raise_hard_ceiling      an accept leaves the hard ceiling identical
     soft_budget_never_exceeds_hard_ceiling              `soft ≤ hard` is preserved by observe + tighten
     invalid_response_emits_no_accepted_observation       a rejected/empty/refusal response emits 0, no raise
     validated_calls_emit_at_most_one_accepted_observation  one response ⇒ at most one Accepted

   The `Option Nat` ceiling forms a lattice with `none` as ⊤ (unbounded): moving
   from `none` to `some c` is a TIGHTENING, never an increase — faithful to the
   Rust, where an unknown window imposes no bound and a learned/recovered window
   only ever narrows it.

   ## Deliberately NOT modelled here (other kernels / obligations)

   - The EMISSION-ordering property "Accepted is emitted AFTER whole-batch
     validation and BEFORE the first tool side effect" is a control-flow property
     of the Rust loops, covered by the `observation_hook_tests` callback tests; this
     pure kernel models only the classifier's verdict (`isAccepted (validatedCalls
     _) = true`, `isAccepted malformed/correlationImpossible = false`) that the
     emit is gated on.
   - Calibration (chars/4 ↔ real tokens) and the truncation-suspect gate are the
     `send_budget.rs` unit tests' domain.

   No Mathlib; bare toolchain; fully machine-checked with no proof holes. -/

namespace NewtPolicy.ResponsesUsage

/-! ### Ceilings: `Option Nat` with `none` as the unbounded top. -/

/-- `leC x c` — "the soft value `x` does not exceed the ceiling `c`", vacuously
    true when the ceiling is unbounded (`none`). The `soft ≤ hard` invariant. -/
def leC (x : Nat) : Option Nat → Prop
  | none => True
  | some h => x ≤ h

/-- `leCeil a b` — the ceiling order with `none` as ⊤: everything is ≤ unbounded,
    unbounded is ≤ nothing finite, finites compare numerically. "Hard never
    increases" is `leCeil newHard oldHard`. -/
def leCeil : Option Nat → Option Nat → Prop
  | _, none => True
  | none, some _ => False
  | some x, some y => x ≤ y

/-- `capTo c x` — clamp `x` to the ceiling `c` (`none` = no clamp). Mirrors the
    Rust `capped_accepted_prompt_tokens`: `x.min(ceiling.unwrap_or(MAX))`. -/
def capTo : Option Nat → Nat → Nat
  | none, x => x
  | some h, x => Nat.min x h

/-- The tightened ceiling after a context-window 400 reports `recovered`: the
    tighter of the recovered window and any prior learned ceiling. -/
def newHardCeiling (recovered : Nat) : Option Nat → Nat
  | none => recovered
  | some h => Nat.min recovered h

/-- `leCeil` is reflexive — an unchanged ceiling has not increased. -/
theorem leCeil_refl (c : Option Nat) : leCeil c c := by
  cases c with
  | none => trivial
  | some x => exact Nat.le_refl x

/-- A clamped value never exceeds the ceiling it was clamped to. -/
theorem capTo_le (c : Option Nat) (x : Nat) : leC (capTo c x) c := by
  cases c with
  | none => trivial
  | some h => simp only [capTo, leC]; exact Nat.min_le_right x h

/-- `leC` is preserved by `Nat.max` when both operands satisfy it — a raise to the
    max of the current budget and a clamped accept stays under the ceiling. -/
theorem leC_max (a b : Nat) (c : Option Nat) (ha : leC a c) (hb : leC b c) :
    leC (Nat.max a b) c := by
  cases c with
  | none => trivial
  | some h => simp only [leC] at ha hb ⊢; exact Nat.max_le.mpr ⟨ha, hb⟩

/-! ### Responses and the accept classifier. -/

/-- One provider response, classified by the emission rules. `validatedCalls` is a
    FULLY-VALIDATED tool-call batch; `malformed` is a content-invalid batch (RR1)
    and `correlationImpossible` a missing/duplicate-id batch (RR2) — the
    fail-closed classes that are NOT provider-accept evidence. -/
inductive Response where
  | usableText (prompt : Nat)
  | validatedCalls (prompt : Nat)
  | empty
  | malformed
  | correlationImpossible
  | refusal
  deriving DecidableEq, Repr

/-- The quality gate: an `Accepted` observation is emitted ONLY for completed
    usable text or a fully-validated tool-call batch. -/
def isAccepted : Response → Bool
  | .usableText _ => true
  | .validatedCalls _ => true
  | _ => false

/-- The backend-reported prompt size carried by an accepted response (0 for the
    rejected classes, which never reach the raise). -/
def promptOf : Response → Nat
  | .usableText p => p
  | .validatedCalls p => p
  | _ => 0

/-- The budget state learned across one turn. -/
structure BudgetState where
  hardCeiling : Option Nat
  softBudget : Nat
  deriving DecidableEq, Repr

/-- The preserved invariant: the soft budget never exceeds the hard ceiling. -/
def Inv (s : BudgetState) : Prop := leC s.softBudget s.hardCeiling

/-- OBSERVE one response. On an accepted response, raise the soft budget to the max
    of its current value and the clamped accept — NEVER touching the hard ceiling;
    on any rejected/empty/refusal response, leave the state untouched. -/
def observe (r : Response) (s : BudgetState) : BudgetState :=
  if isAccepted r then
    { s with softBudget := Nat.max s.softBudget (capTo s.hardCeiling (promptOf r)) }
  else s

/-- The count of `Accepted` observations one response emits — 0 or 1. -/
def acceptedEmitted (r : Response) : Nat := if isAccepted r then 1 else 0

/-- A context-window 400: tighten the hard ceiling to `newHardCeiling` and collapse
    the soft budget onto it (fail-closed), mirroring the Rust cw-400 recovery
    (`send_budget = effective_input_ceiling = Some(new_budget)`). -/
def tighten (recovered : Nat) (s : BudgetState) : BudgetState :=
  { hardCeiling := some (newHardCeiling recovered s.hardCeiling),
    softBudget := newHardCeiling recovered s.hardCeiling }

/-! ### The five machine-checked laws. -/

/-- OBSERVE leaves the hard ceiling byte-identical (it only ever moves the soft
    budget). This is `accepted_observation_cannot_raise_hard_ceiling`. -/
theorem observe_preserves_hard (r : Response) (s : BudgetState) :
    (observe r s).hardCeiling = s.hardCeiling := by
  simp only [observe]; split <;> rfl

/-- (LAW) An accepted observation cannot raise the hard ceiling — it cannot change
    it at all. -/
theorem accepted_observation_cannot_raise_hard_ceiling (r : Response) (s : BudgetState) :
    (observe r s).hardCeiling = s.hardCeiling :=
  observe_preserves_hard r s

/-- A cw-400 only tightens the hard ceiling — the new ceiling is ≤ the old one in
    the `none`-as-⊤ order. -/
theorem tighten_hard_never_increases (k : Nat) (s : BudgetState) :
    leCeil (tighten k s).hardCeiling s.hardCeiling := by
  show leCeil (some (newHardCeiling k s.hardCeiling)) s.hardCeiling
  cases hc : s.hardCeiling with
  | none => trivial
  | some h => simp only [newHardCeiling, leCeil]; exact Nat.min_le_right k h

/-- (LAW) The hard ceiling NEVER increases during a turn: neither an accepted
    observation nor a cw-400 tightening raises it. -/
theorem hard_ceiling_never_increases (s : BudgetState) :
    (∀ r, leCeil (observe r s).hardCeiling s.hardCeiling) ∧
      (∀ k, leCeil (tighten k s).hardCeiling s.hardCeiling) := by
  refine ⟨fun r => ?_, fun k => tighten_hard_never_increases k s⟩
  rw [observe_preserves_hard]; exact leCeil_refl s.hardCeiling

/-- OBSERVE preserves `soft ≤ hard`. -/
theorem observe_preserves_inv (r : Response) (s : BudgetState) (hs : Inv s) :
    Inv (observe r s) := by
  simp only [observe]
  split
  · show leC (Nat.max s.softBudget (capTo s.hardCeiling (promptOf r))) s.hardCeiling
    exact leC_max _ _ _ hs (capTo_le _ _)
  · exact hs

/-- A cw-400 establishes `soft ≤ hard` outright: it sets both to the tightened
    ceiling. (Does not even need `Inv s`.) -/
theorem tighten_preserves_inv (k : Nat) (s : BudgetState) : Inv (tighten k s) := by
  show leC (newHardCeiling k s.hardCeiling) (some (newHardCeiling k s.hardCeiling))
  exact Nat.le_refl _

/-- (LAW) `soft ≤ hard` is an INVARIANT preserved by both observe and cw-400: a
    success may raise the soft budget but never past the hard ceiling, and a cw-400
    keeps them consistent. -/
theorem soft_budget_never_exceeds_hard_ceiling (s : BudgetState) (hs : Inv s) :
    (∀ r, Inv (observe r s)) ∧ (∀ k, Inv (tighten k s)) :=
  ⟨fun r => observe_preserves_inv r s hs, fun k => tighten_preserves_inv k s⟩

/-- (LAW) A response the classifier rejects — a content-invalid batch (RR1), a
    correlation-impossible batch (RR2), an empty response, or a refusal — emits NO
    `Accepted` observation and leaves the budget state untouched (no invented
    measurement, no ratchet). -/
theorem invalid_response_emits_no_accepted_observation (r : Response) (s : BudgetState)
    (hinv : isAccepted r = false) : acceptedEmitted r = 0 ∧ observe r s = s := by
  refine ⟨?_, ?_⟩
  · simp only [acceptedEmitted, hinv]; rfl
  · simp only [observe, hinv]; rfl

/-- The rejected classes concretely reject: none of them is accept evidence. -/
theorem malformed_not_accepted : isAccepted .malformed = false := rfl
theorem correlation_impossible_not_accepted : isAccepted .correlationImpossible = false := rfl
theorem empty_not_accepted : isAccepted .empty = false := rfl
theorem refusal_not_accepted : isAccepted .refusal = false := rfl

/-- One response emits at most one `Accepted` observation. -/
theorem accepted_emitted_le_one (r : Response) : acceptedEmitted r ≤ 1 := by
  simp only [acceptedEmitted]; split <;> omega

/-- (LAW) A fully-validated tool-call batch emits EXACTLY one `Accepted`
    observation, and NO response emits more than one — at most one Accepted per
    response. -/
theorem validated_calls_emit_at_most_one_accepted_observation (p : Nat) :
    acceptedEmitted (.validatedCalls p) = 1 ∧ ∀ r, acceptedEmitted r ≤ 1 :=
  ⟨rfl, accepted_emitted_le_one⟩

/-! ### Non-vacuity: the laws firing on concrete states.

    `demo` has a learned hard ceiling of 8,000 and a soft budget of 2,000. These
    exhibit an accepted round clamping the raise, a rejected round changing
    nothing, and the headline "accepted evidence after a cw-400 stays clamped". -/

def demo : BudgetState := { hardCeiling := some 8000, softBudget := 2000 }

/-- A validated tool batch reporting a 20,000-token prompt raises the soft budget
    ONLY to the 8,000 hard ceiling — never past it. -/
example : (observe (.validatedCalls 20000) demo).softBudget = 8000 := by decide

/-- …and it leaves the hard ceiling exactly where it was. -/
example : (observe (.validatedCalls 20000) demo).hardCeiling = some 8000 := by decide

/-- A content-invalid batch changes nothing — no raise, no ratchet. -/
example : observe .malformed demo = demo := by decide

/-- The headline B4 property: after a cw-400 tightens the ceiling to 4,000, a later
    accepted 20,000-token round cannot raise the tightened hard ceiling (it stays
    4,000) and the soft budget stays ≤ it. -/
example : (observe (.validatedCalls 20000) (tighten 4000 demo)).hardCeiling = some 4000 := by
  decide

example : Inv (observe (.validatedCalls 20000) (tighten 4000 demo)) :=
  observe_preserves_inv _ _ (tighten_preserves_inv 4000 demo)

/-- Multiple successful rounds raise ONLY the soft budget; the hard ceiling is
    unchanged across the whole sequence. -/
example :
    (observe (.usableText 5000) (observe (.validatedCalls 3000) demo)).hardCeiling
      = some 8000 := by decide

end NewtPolicy.ResponsesUsage
