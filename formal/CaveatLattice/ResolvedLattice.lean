/-
  ResolvedLattice — the resolved-authority join-semilattice + L3 admission
  soundness, machine-checked.

  Formal companion to `agent-mesh-protocol`'s `authority.rs` (`ResolvedScope`,
  `ResolvedScope::union`, `admit`). What the PR-#72 proptests CHECK on random
  inputs, these theorems PROVE for all inputs — the behavioral-constitution
  pairing: proptest = empirical; Lean = universal.

  The #317 audit showed enforcement admitted a *wider* scope than authorized.
  The fix rests on the algebra here: `union` is a genuine least-upper-bound, so
  the bound `delegated ⊔ closure` is the tightest honest bound, and admission =
  `resolved ⊑ bound` — a widening is refused, never filed under a weaker strength.
-/

namespace ResolvedLattice

/-- Resolved authority a native fence actually permits on one axis. Mirrors the
    Rust `ResolvedScope = Bounded{concrete,classes} | Unbounded | Unknown`; the
    two `Bounded` dimensions are modeled as membership predicates (like
    `Scope.only` in `CaveatLattice.Basic`). -/
inductive RScope (α : Type) where
  | bounded (concrete : α → Prop) (classes : α → Prop)
  | unbounded
  | unknown

namespace RScope

/-- What a resolved scope permits. `unbounded` and `unknown` are the top of the
    *authority* order (conservatively permit everything); the DECIDABILITY
    difference between them is an admission concern, not an authority one. -/
def permits {α} : RScope α → α → Prop
  | bounded c l, x => c x ∨ l x
  | unbounded,   _ => True
  | unknown,     _ => True

/-- The union (join). `unknown` absorbs; else `unbounded` absorbs; else the two
    `Bounded` dimensions unite pointwise. Mirrors `ResolvedScope::union` exactly. -/
def union {α} : RScope α → RScope α → RScope α
  | unknown,       _             => unknown
  | _,             unknown       => unknown
  | unbounded,     _             => unbounded
  | _,             unbounded     => unbounded
  | bounded c1 l1, bounded c2 l2 => bounded (fun x => c1 x ∨ c2 x) (fun x => l1 x ∨ l2 x)

/-- Bottom: permits nothing; the identity for `union` (the `{},{}` empty scope). -/
def bottom {α} : RScope α := bounded (fun _ => False) (fun _ => False)

/-- Attenuation order (⊑): `a` permits no more than `b`. -/
def le {α} (a b : RScope α) : Prop := ∀ x, permits a x → permits b x

@[inherit_doc] scoped infix:50 " ⊑ " => le

/-- Authority-equivalence (the semilattice laws hold up to this, not `=`). -/
def equiv {α} (a b : RScope α) : Prop := le a b ∧ le b a

@[inherit_doc] scoped infix:50 " ≈ " => equiv

theorem le_refl {α} (a : RScope α) : a ⊑ a := fun _ h => h

theorem le_trans {α} {a b c : RScope α} (h₁ : a ⊑ b) (h₂ : b ⊑ c) : a ⊑ c :=
  fun x hx => h₂ x (h₁ x hx)

/-- The definitional bridge: `permits (union a b) x ↔ permits a x ∨ permits b x`. -/
theorem permits_union {α} (a b : RScope α) (x : α) :
    permits (union a b) x ↔ (permits a x ∨ permits b x) := by
  cases a <;> cases b <;>
    simp only [union, permits, true_or, or_true, or_comm, or_left_comm, iff_self]

/-- **`union_is_an_upper_bound` — the no-widening law, left.** The join never
    grants less than either operand. -/
theorem le_union_left {α} (a b : RScope α) : a ⊑ union a b :=
  fun x hx => (permits_union a b x).mpr (Or.inl hx)

/-- The no-widening law, right. -/
theorem le_union_right {α} (a b : RScope α) : b ⊑ union a b :=
  fun x hx => (permits_union a b x).mpr (Or.inr hx)

/-- The join is the *least* upper bound: any `c` above both `a` and `b` is above
    their union. With `le_union_*` this makes `union` a genuine join. -/
theorem union_le {α} (a b c : RScope α) (ha : a ⊑ c) (hb : b ⊑ c) : union a b ⊑ c := by
  intro x hx
  rcases (permits_union a b x).mp hx with h | h
  · exact ha x h
  · exact hb x h

/-- `union_is_commutative` (up to ≈). -/
theorem union_comm {α} (a b : RScope α) : union a b ≈ union b a := by
  refine ⟨?_, ?_⟩ <;> intro x hx <;>
    · rw [permits_union] at hx ⊢; exact hx.symm

/-- `union_is_idempotent`. -/
theorem union_idem {α} (a : RScope α) : union a a ≈ a := by
  refine ⟨?_, le_union_left a a⟩
  intro x hx; rcases (permits_union a a x).mp hx with h | h <;> exact h

/-- `union_is_associative` (up to ≈). -/
theorem union_assoc {α} (a b c : RScope α) :
    union (union a b) c ≈ union a (union b c) := by
  refine ⟨?_, ?_⟩ <;> intro x hx <;> simp only [permits_union] at hx ⊢
  · exact or_assoc.mp hx
  · exact or_assoc.mpr hx

/-- `bottom` permits nothing — both `Bounded` dimensions are the empty predicate. -/
theorem not_permits_bottom {α} (x : α) : ¬ permits (bottom : RScope α) x := by
  intro h
  rcases h with f | f <;> exact f

/-- `empty_is_the_union_identity`: `bottom` is the identity for `union`. -/
theorem bottom_union {α} (a : RScope α) : union bottom a ≈ a := by
  refine ⟨?_, le_union_right bottom a⟩
  intro x hx
  rcases (permits_union bottom a x).mp hx with hb | ha
  · exact absurd hb (not_permits_bottom x)
  · exact ha

/-- `unknown_propagates_through_union`: `Unknown` absorbs (structural equality). -/
theorem unknown_union {α} (a : RScope α) : union unknown a = unknown := rfl

/-- `unbounded_absorbs_bounded`: `Unbounded ∪ Bounded = Unbounded` (structural). -/
theorem unbounded_union_bounded {α} (c l : α → Prop) :
    union unbounded (bounded c l) = unbounded := rfl

end RScope

/-! ### L3 admission soundness

`admit` accepts an axis iff the fence's resolved scope stays within the bound
`delegated ⊔ closure`. The theorems below are the pure-function content of
`authority::admit` and the honest-parity mechanism. -/

open RScope

/-- The per-axis admission decision (the `relate ∈ {Equal, Subset}` accept set in
    the Rust: the fence is within the bound). -/
def admits {α} (resolved delegated closure : RScope α) : Prop :=
  resolved ⊑ union delegated closure

/-- **Soundness (L3 / INV-BOUND).** An admitted fence never exceeds the bound —
    `delegated ⊔ closure` — it was admitted against. No admitted child has
    authority outside `delegated ∪ closure`. -/
theorem admits_sound {α} (resolved delegated closure : RScope α)
    (h : admits resolved delegated closure) : resolved ⊑ union delegated closure := h

/-- A fence within the delegated grant alone is always admitted (needs no
    closure) — via `le_union_left` + transitivity. -/
theorem admits_within_delegated {α} (resolved delegated closure : RScope α)
    (h : resolved ⊑ delegated) : admits resolved delegated closure :=
  le_trans h (le_union_left delegated closure)

/-- **The honest-parity mechanism.** A fence a delegated grant does NOT authorize
    on its own can still be admitted — but ONLY because the explicit `closure`
    authorizes it (`resolved ⊑ closure`), never by silent widening. -/
theorem admits_via_closure {α} (resolved delegated closure : RScope α)
    (h : resolved ⊑ closure) : admits resolved delegated closure :=
  le_trans h (le_union_right delegated closure)

/-- **No silent widening (contrapositive of soundness).** A fence that exceeds
    the bound `delegated ⊔ closure` is refused — it cannot be admitted. -/
theorem widening_refused {α} (resolved delegated closure : RScope α)
    (h : ¬ (resolved ⊑ union delegated closure)) : ¬ admits resolved delegated closure := h

end ResolvedLattice
