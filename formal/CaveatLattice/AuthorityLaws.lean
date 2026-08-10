/-
  AuthorityLaws — L4 FLOOR and L6 AUTHORIZATION, machine-checked.

  Companions to `ResolvedLattice` (L3 BOUND). Together L3+L4+L6 are the pure-
  function content of admission and delegation in `agent-mesh`'s `authority.rs`
  and the agent-bridle admission path.

  - **L4 FLOOR** — every restricted axis meets its required strength, and strength
    is ORTHOGONAL to scope. The headline theorem `floor_does_not_imply_bound`
    turns the #317 audit finding into a proof: passing the strength floor does NOT
    imply the scope is bounded, so an admission that checks ONLY the floor (as
    `unenforceable_axis_in_report` did — `strength ≥ floor`, no scope operand) is
    UNSOUND. Scope (L3) and strength (L4) must BOTH be checked.

  - **L6 AUTHORIZATION** — authority only attenuates (`⊑`) across a delegation edge;
    the sole way a child may exceed its parent is a signed operator elevation.
    `widening_requires_signed_elevation` states it exactly: an admitted edge that
    lets the child exceed its parent MUST be a signed elevation. Mirrors mesh #72's
    `verify_chain` + `DenyAllElevations` default (row 5 `ElevationUnauthorized`).
    A CID identifies; it never authorizes.
-/

import CaveatLattice.ResolvedLattice

namespace AuthorityLaws

open ResolvedLattice
open ResolvedLattice.RScope

/-! ### L4 FLOOR — strength is orthogonal to scope. -/

/-- A restricted axis meets its floor iff its actual enforcement strength is at
    least the required one. Strength is a linear order (modeled as `Nat`: a higher
    number is a stronger mechanism, e.g. `Unenforced < Advisory < Kernel`). -/
def meetsFloor (actual required : Nat) : Prop := required ≤ actual

/-- **(LAW L4)** An axis that meets its floor has actual strength ≥ required. The
    floor is exactly the strength obligation — and NOTHING about scope. -/
theorem floor_sound {actual required : Nat} (h : meetsFloor actual required) :
    required ≤ actual := h

/-- **(LAW L4 — the #317 defect, as a theorem)** Passing the strength floor does
    NOT imply the scope is bounded. There is an axis that meets its floor
    (`actual = required = 0`) yet whose resolved scope (`unbounded`) escapes the
    bound `delegated ⊔ closure` (`bottom ⊔ bottom`, which permits nothing). Hence
    strength-floor and scope-bound are INDEPENDENT: an admission checking only the
    floor cannot express INV-BOUND, and is unsound. This is precisely the class of
    19 violations the bounded-authority audit found. -/
theorem floor_does_not_imply_bound :
    ∃ (actual required : Nat) (resolved delegated closure : RScope Bool),
      meetsFloor actual required ∧ ¬ admits resolved delegated closure := by
  refine ⟨0, 0, unbounded, bottom, bottom, Nat.le_refl 0, ?_⟩
  intro h
  -- `h : admits unbounded bottom bottom` = `unbounded ⊑ union bottom bottom`.
  -- Instantiate at `true`: `unbounded` permits it (top), so the union must too…
  have hp : permits (union bottom (bottom : RScope Bool)) true := h true True.intro
  -- …but `union bottom bottom ≈ bottom`, which permits nothing. Contradiction.
  exact not_permits_bottom true ((bottom_union bottom).1 true hp)

/-! ### L6 AUTHORIZATION — attenuation-only, unless a signed elevation. -/

/-- A delegation edge: either an attenuation (child must stay `⊑` parent) or an
    elevation carrying `signed` = "a configured operator verifier accepted the
    attestation" (mesh #72's `verifier`). Authorization comes from the signature,
    never from the edge merely existing. -/
inductive Edge where
  | attenuate
  | elevate (signed : Bool)

/-- When an edge is admissible for a given child/parent authority. -/
def edgeOk {α} (child parent : RScope α) : Edge → Prop
  | Edge.attenuate      => child ⊑ parent
  | Edge.elevate signed => signed = true

/-- **(LAW L6a)** An admitted attenuation edge never widens: `child ⊑ parent`. -/
theorem attenuation_never_widens {α} (child parent : RScope α)
    (h : edgeOk child parent Edge.attenuate) : child ⊑ parent := h

/-- **(LAW L6b)** Across a pure-attenuation path, authority only shrinks — the
    grandchild is bounded by the grandparent (transitivity of `⊑`). -/
theorem attenuation_path_bound {α} {a b c : RScope α}
    (hba : b ⊑ a) (hcb : c ⊑ b) : c ⊑ a := le_trans hcb hba

/-- **(LAW L6c)** An UNSIGNED elevation is never admissible — a CID identifies, it
    does not authorize; only a valid operator signature authorizes widening. -/
theorem unsigned_elevation_refused {α} (child parent : RScope α) :
    ¬ edgeOk child parent (Edge.elevate false) := by
  simp [edgeOk]

/-- **(LAW L6 — the authorization law)** If an admitted edge lets the child EXCEED
    its parent (`¬ child ⊑ parent`), that edge MUST be a signed elevation.
    Attenuation cannot widen, and an unsigned elevation is inadmissible — so the
    only authority-increasing edge is `elevate true`. -/
theorem widening_requires_signed_elevation {α} (child parent : RScope α) (e : Edge)
    (hok : edgeOk child parent e) (hwiden : ¬ child ⊑ parent) :
    ∃ s, e = Edge.elevate s ∧ s = true := by
  cases e with
  | attenuate => exact absurd hok hwiden
  | elevate s =>
      cases s with
      | false => simp [edgeOk] at hok
      | true  => exact ⟨true, rfl, rfl⟩

end AuthorityLaws
