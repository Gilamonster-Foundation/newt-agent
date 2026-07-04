/-
  CaveatLattice — a machine-checked specification of the OCAP caveat lattice.

  This is the hand-authored Lean 4 model of the algebra that
  `agent-mesh-protocol`'s `Caveats` / `Scope` / `meet` implement and that
  agent-bridle enforces and newt composes (issue #902). It is deliberately
  self-contained (no Mathlib) so it checks with a bare `lean`; the `meet`
  semantics mirror the Rust (`Scope::All` = top, `Scope::Only(set)` = an
  allow-list, `meet` = intersection of authority).

  The point (per docs/vision.md): the property tests SAMPLE these laws; here
  they are THEOREMS — total, machine-checked. The keystone is
  attenuation-only: a `meet` (and therefore any delegation / sub-agent / tool
  call) can NEVER amplify authority. The confused-deputy bound falls out as a
  one-line corollary.
-/

namespace Ocap

/-- The permission scope for one capability axis over items of type `α`
    (a host, a path, a command). `all` permits everything; `only p` permits
    exactly the items satisfying the predicate `p` (the Rust `Only(set)`,
    modeled set-theoretically as its membership predicate). -/
inductive Scope (α : Type) where
  | all
  | only (p : α → Prop)

namespace Scope

/-- Whether a scope permits an item — the authority a scope actually grants. -/
def permits {α} : Scope α → α → Prop
  | all,    _ => True
  | only p, x => p x

/-- The meet: the greatest authority `⊑` both — intersection of allow-lists,
    with `all` as the identity. Mirrors `Scope::meet` in the Rust. -/
def meet {α} : Scope α → Scope α → Scope α
  | all,    b      => b
  | only p, all    => only p
  | only p, only q => only (fun x => p x ∧ q x)

/-- Attenuation order: `a ⊑ b` iff `a` grants no more than `b` — every item
    `a` permits, `b` permits too. This is the ONLY order that matters for
    least-authority. -/
def le {α} (a b : Scope α) : Prop := ∀ x, permits a x → permits b x

@[inherit_doc] scoped infix:50 " ⊑ " => le

/-- `meet a b` permits an item iff BOTH `a` and `b` do. The definitional
    bridge every proof below leans on. -/
theorem permits_meet {α} (a b : Scope α) (x : α) :
    permits (meet a b) x ↔ (permits a x ∧ permits b x) := by
  cases a with
  | all => cases b <;> simp [meet, permits]
  | only p => cases b <;> simp [meet, permits]

/-- **Attenuation-only, left (the keystone).** A meet never grants more than its
    left operand: a delegation cannot amplify the caller's authority. -/
theorem meet_le_left {α} (a b : Scope α) : meet a b ⊑ a := by
  intro x hx
  exact ((permits_meet a b x).mp hx).1

/-- Attenuation-only, right (symmetric). -/
theorem meet_le_right {α} (a b : Scope α) : meet a b ⊑ b := by
  intro x hx
  exact ((permits_meet a b x).mp hx).2

/-- The meet is the greatest lower bound: anything `⊑` both `a` and `b` is
    `⊑` their meet. With the two `meet_le_*` this makes `meet` the genuine
    lattice meet (not merely *a* lower bound). -/
theorem le_meet {α} (a b c : Scope α) (hab : c ⊑ a) (hbc : c ⊑ b) : c ⊑ meet a b := by
  intro x hx
  exact (permits_meet a b x).mpr ⟨hab x hx, hbc x hx⟩

/-- `all` is the top of the order — the unrestricted grant. -/
theorem le_all {α} (a : Scope α) : a ⊑ all := by
  intro x _; trivial

theorem le_refl {α} (a : Scope α) : a ⊑ a := fun _ h => h

theorem le_trans {α} {a b c : Scope α} (h₁ : a ⊑ b) (h₂ : b ⊑ c) : a ⊑ c :=
  fun x hx => h₂ x (h₁ x hx)

/-- Two scopes permitting exactly the same items — the equivalence the
    semilattice laws hold up to (structural `=` is too strong, since `only p`
    and `only q` with `p ↔ q` grant the same authority). -/
def equiv {α} (a b : Scope α) : Prop := a ⊑ b ∧ b ⊑ a

@[inherit_doc] scoped infix:50 " ≈ " => equiv

/-- The meet is commutative (up to authority-equivalence). -/
theorem meet_comm {α} (a b : Scope α) : meet a b ≈ meet b a := by
  refine ⟨?_, ?_⟩ <;> intro x hx <;>
    · rw [permits_meet] at *; exact ⟨hx.2, hx.1⟩

/-- The meet is idempotent (up to authority-equivalence). -/
theorem meet_idem {α} (a : Scope α) : meet a a ≈ a := by
  refine ⟨meet_le_left a a, ?_⟩
  intro x hx; exact (permits_meet a a x).mpr ⟨hx, hx⟩

/-- The meet is associative (up to authority-equivalence). -/
theorem meet_assoc {α} (a b c : Scope α) : meet (meet a b) c ≈ meet a (meet b c) := by
  constructor
  · intro x hx
    simp only [permits_meet] at hx ⊢
    exact ⟨hx.1.1, hx.1.2, hx.2⟩
  · intro x hx
    simp only [permits_meet] at hx ⊢
    exact ⟨⟨hx.1, hx.2.1⟩, hx.2.2⟩

end Scope

/-! ### Caveats: the product lattice, and the confused-deputy bound -/

open Scope

/-- A capability grant across the axes newt/agent-bridle enforce (fs read/write,
    exec, net), each a `Scope` over `String` (paths, command names, hosts). The
    real `Caveats` also carries `max_calls` / `valid_for_generation`; those are
    separate lattice components, elided here to keep the first spec on the axis
    scopes — the load-bearing ones for authority. -/
structure Caveats where
  fsRead  : Scope String
  fsWrite : Scope String
  exec    : Scope String
  net     : Scope String

namespace Caveats

/-- Component-wise meet — the real `Caveats::meet`. -/
def meet (a b : Caveats) : Caveats :=
  { fsRead  := Scope.meet a.fsRead  b.fsRead
    fsWrite := Scope.meet a.fsWrite b.fsWrite
    exec    := Scope.meet a.exec    b.exec
    net     := Scope.meet a.net     b.net }

/-- Attenuation order on caveats: `a ⊑ b` iff `a` is `⊑ b` on EVERY axis —
    `a` grants no more than `b` anywhere. -/
def le (a b : Caveats) : Prop :=
  a.fsRead ⊑ b.fsRead ∧ a.fsWrite ⊑ b.fsWrite ∧ a.exec ⊑ b.exec ∧ a.net ⊑ b.net

@[inherit_doc] scoped infix:50 " ⊑ " => le

/-- **Attenuation-only for the whole grant.** `meet a b` grants no more than
    `a` on any axis — the invariant the enforcement floor, the re-mint, and
    every delegation depend on. -/
theorem meet_le_left (a b : Caveats) : meet a b ⊑ a :=
  ⟨Scope.meet_le_left _ _, Scope.meet_le_left _ _,
   Scope.meet_le_left _ _, Scope.meet_le_left _ _⟩

theorem meet_le_right (a b : Caveats) : meet a b ⊑ b :=
  ⟨Scope.meet_le_right _ _, Scope.meet_le_right _ _,
   Scope.meet_le_right _ _, Scope.meet_le_right _ _⟩

theorem le_refl (a : Caveats) : a ⊑ a :=
  ⟨Scope.le_refl _, Scope.le_refl _, Scope.le_refl _, Scope.le_refl _⟩

theorem le_trans {a b c : Caveats} (h₁ : a ⊑ b) (h₂ : b ⊑ c) : a ⊑ c :=
  ⟨Scope.le_trans h₁.1 h₂.1, Scope.le_trans h₁.2.1 h₂.2.1,
   Scope.le_trans h₁.2.2.1 h₂.2.2.1, Scope.le_trans h₁.2.2.2 h₂.2.2.2⟩

/-- **The confused-deputy bound (docs/vision.md, #902).** A delegation hands the
    caller's authority meet-attenuated by a grant `g`; the result is always `⊑`
    the caller. Since *sub-agent = tool call = bounded delegation*, this is the
    single theorem that certifies delegation-as-tool-calling never leaks
    authority: whatever a dispatched crew / sub-agent / re-minted tool call
    receives, it can act with no more than its caller. -/
theorem confused_deputy_bound (caller grant : Caveats) : meet caller grant ⊑ caller :=
  meet_le_left caller grant

/-- Corollary: attenuation composes. Any chain of delegations
    (caller → g₁ → g₂) stays `⊑` the original caller — the *global* property a
    sampling property-test cannot certify but a proof does. -/
theorem delegation_chain_bounded (caller g₁ g₂ : Caveats) :
    meet (meet caller g₁) g₂ ⊑ caller :=
  le_trans (meet_le_left _ _) (meet_le_left _ _)

end Caveats

end Ocap
