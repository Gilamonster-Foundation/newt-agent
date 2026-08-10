/-
  IdentityLaw — L1 IDENTITY, machine-checked.

  id = CID of the object's ONE validated canonical typed representation. Two things
  must hold, and this module proves both — non-vacuously:

    (a) the id is taken over the NORMALIZED body, not the raw wire bytes — so two
        inputs that MEAN the same thing (the canonical example: `{x:null}` vs `{}`)
        get the SAME id, and a hypothetical raw-byte hash is shown UNSOUND;
    (b) the id carries a DOMAIN TAG, so a CID of one type can never be confused for
        another (`typed_domains_cannot_collide`).

  Mirrors mesh #72's `identity_is_transport_agnostic` (row 12) and
  `typed_domains_cannot_collide` (row 11). Uses the same abstraction as the TLA+
  `Cid` axiom (identity = value) so the two provers agree; the CONTENT here is
  *what* goes into the id — normalize-then-tag, never raw bytes.
-/

namespace IdentityLaw

/-- Security-object domains. A CID names exactly one of these. -/
inductive Kind where
  | authority | grant | plan | evidence | result | attestation
deriving DecidableEq

/-- A normalized canonical body. Modeled minimally: the only thing L1 cares about is
    whether two raw inputs CONVERGE to the same canonical form. -/
structure Canon where
  form : Nat
deriving DecidableEq

/-- Raw wire input, before decode/validate/normalize. Two shapes that mean the same
    thing — an explicit null field vs its absence — are the canonical example. -/
inductive Raw where
  | withExplicitNull
  | withoutField
  | other (n : Nat)
deriving DecidableEq

/-- Canonicalize: decode → validate → normalize → re-encode. `{x:null}` and `{}`
    both normalize to the empty canonical form; any other value keeps its identity. -/
def normalize : Raw → Canon
  | Raw.withExplicitNull => ⟨0⟩
  | Raw.withoutField     => ⟨0⟩
  | Raw.other n          => ⟨n + 1⟩

/-- A content id: the domain tag PLUS the normalized canonical body. Identity =
    value (matching the TLA+ `Cid` axiom); the point is WHAT is committed to. -/
structure Cid where
  kind : Kind
  body : Canon
deriving DecidableEq

/-- The id of a typed object: tag + normalized body. NEVER the raw wire bytes. -/
def cid (k : Kind) (r : Raw) : Cid := ⟨k, normalize r⟩

/-- **(LAW L1a) canonical convergence.** Two raw inputs that normalize to the same
    canonical body get the SAME id under the same domain — even when their raw bytes
    differ. This is WHY the id is taken over the normal form, not the wire bytes. -/
theorem canonical_convergence {k : Kind} {r₁ r₂ : Raw}
    (h : normalize r₁ = normalize r₂) : cid k r₁ = cid k r₂ := by
  simp [cid, h]

/-- The convergence is REAL, not vacuous: `{x:null}` and `{}` are DISTINCT raw
    inputs that share one id. -/
theorem convergence_is_nonvacuous :
    Raw.withExplicitNull ≠ Raw.withoutField ∧
      cid Kind.grant Raw.withExplicitNull = cid Kind.grant Raw.withoutField :=
  ⟨by decide, rfl⟩

/-- **(LAW L1b) raw-byte hashing is unsound.** A hypothetical id that hashed the RAW
    input would give two semantically-equal objects DIFFERENT ids — violating
    "equal objects ⇒ equal id". Hence L1 mandates normalize-then-tag. (The identity
    analogue of the #317 "defect as a theorem".) -/
theorem raw_hashing_unsound :
    ∃ r₁ r₂ : Raw, normalize r₁ = normalize r₂ ∧ r₁ ≠ r₂ :=
  ⟨Raw.withExplicitNull, Raw.withoutField, rfl, by decide⟩

/-- **(LAW L1c) identity pins the typed rep.** Equal ids ⇒ same domain AND same
    canonical body. An id identifies exactly one validated typed object. -/
theorem identity_pins_typed_rep {k₁ k₂ : Kind} {r₁ r₂ : Raw}
    (h : cid k₁ r₁ = cid k₂ r₂) : k₁ = k₂ ∧ normalize r₁ = normalize r₂ := by
  simp only [cid, Cid.mk.injEq] at h
  exact h

/-- **(LAW L1d) typed domains cannot collide.** Different domains ⇒ different ids;
    a CID of one type is NEVER accepted as another (the newtype / domain-tag law —
    #72's `GrantId(authority_id.0)` no longer compiles). -/
theorem typed_domains_cannot_collide {k₁ k₂ : Kind} {r₁ r₂ : Raw}
    (hk : k₁ ≠ k₂) : cid k₁ r₁ ≠ cid k₂ r₂ := by
  intro h
  exact hk (identity_pins_typed_rep h).1

end IdentityLaw
