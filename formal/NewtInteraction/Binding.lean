/-
  NewtInteraction.Binding — the A3 response-validation kernel
  (`BHV-INTERACTION-001..003`, see `spec/behavior-map.toml`).

  The renderer-neutral interaction contract: a response answers an offer only
  when EVERY binding it claims matches the offer that was actually published —
  definition, instance, revision, digest, control set, control types, and
  audience — and only then does any action resolve to a handler. This is the
  per-response decision kernel behind
  `newt-interaction/src/binding.rs::validate_response`; the TLC model
  `spec/tla/InteractionLifecycle.tla` checks the surrounding lifecycle
  temporally (exactly-one resolution, terminal-is-terminal, expiry).

  Modelled in the idiom of `NewtPolicy.PromptForm`: the offer carries the
  decision functions rather than the data, so the kernel stays small and needs
  no equality typeclasses. Self-contained (no Mathlib), matching the rest of
  `formal/`.
-/

namespace NewtInteraction
namespace Binding

/-- The A3 offer lifecycle (`newt-interaction/src/instance.rs::LifecycleState`). -/
inductive State where
  | draft
  | published
  | answered
  | cancelled
  | expired
  | unsupported
  deriving DecidableEq, Repr

/-- A published offer, as the validator sees it.

    `Ctrl` is a control id, `Opt` a choice option, `Aud` a responder audience.
    The four `*Matches` fields stand for the identity bindings the validator
    compares (definition id, instance id, revision, and the digest recomputed
    from the definition body); the predicates stand for its set membership
    tests. -/
structure Offer (Ctrl Opt Aud : Type) where
  /-- The offer's lifecycle state at validation time. -/
  state : State
  /-- The response names this offer's definition. -/
  definitionMatches : Bool
  /-- The response names this offer's instance. -/
  instanceMatches : Bool
  /-- The response was minted at the definition's current revision. -/
  revisionMatches : Bool
  /-- The definition hashes to the id the response was bound against. -/
  digestMatches : Bool
  /-- The definition declares this control. -/
  declares : Ctrl → Bool
  /-- The submitted value has the control's declared type. -/
  typeMatches : Ctrl → Opt → Bool
  /-- The offer's responder policy admits this audience. -/
  admits : Aud → Bool
  /-- The caller registered a handler for this option. -/
  registered : Opt → Bool
  /-- That registration admits this audience. -/
  eligible : Opt → Aud → Bool
  /-- Every control the definition marks required is answered. -/
  requiredAnswered : List (Ctrl × Opt) → Bool

/-- A response: who is answering, and what they submitted. -/
structure Response (Ctrl Opt Aud : Type) where
  /-- The audience the responder presented. -/
  audience : Aud
  /-- One `(control, option)` per submission. -/
  values : List (Ctrl × Opt)

/-- Only a published offer is open. Exhaustive on purpose: a new `State`
    variant must be classified here before this compiles. -/
def isPublished : State → Bool
  | State.published => true
  | State.draft | State.answered | State.cancelled
  | State.expired | State.unsupported => false

/-- The identity and policy bindings, checked before any submission is read. -/
def bindingsOk {Ctrl Opt Aud} (o : Offer Ctrl Opt Aud) (r : Response Ctrl Opt Aud) : Bool :=
  isPublished o.state && o.definitionMatches && o.instanceMatches
    && o.revisionMatches && o.digestMatches && o.admits r.audience

/-- One submission, against the control it claims to answer and the handler it
    would route to. -/
def submissionOk {Ctrl Opt Aud} (o : Offer Ctrl Opt Aud) (aud : Aud)
    (s : Ctrl × Opt) : Bool :=
  o.declares s.1 && o.typeMatches s.1 s.2 && o.registered s.2 && o.eligible s.2 aud

/-- Validate a response against the offer it claims to answer.

    Returns the resolved actions on acceptance, or nothing at all. The whole
    submission list stands or falls together — there is no partial result. -/
def validate {Ctrl Opt Aud} (o : Offer Ctrl Opt Aud) (r : Response Ctrl Opt Aud) :
    Option (List (Ctrl × Opt)) :=
  if bindingsOk o r && r.values.all (submissionOk o r.audience) && o.requiredAnswered r.values
  then some r.values
  else none


/-- Acceptance is exactly the conjunction of the three gates, and what comes
    back is exactly what was submitted. The single inversion lemma the theorems
    below share. -/
theorem accepted_inv {Ctrl Opt Aud} {o : Offer Ctrl Opt Aud} {r : Response Ctrl Opt Aud}
    {acts : List (Ctrl × Opt)} (h : validate o r = some acts) :
    bindingsOk o r = true
      ∧ r.values.all (submissionOk o r.audience) = true
      ∧ o.requiredAnswered r.values = true
      ∧ acts = r.values := by
  unfold validate at h
  split at h
  · next hc =>
    simp only [Bool.and_eq_true] at hc
    exact ⟨hc.1.1, hc.1.2, hc.2, by simpa using h.symm⟩
  · next => simp at h

/-- `BHV-INTERACTION-001`: acceptance implies EVERY binding matched — the
    offer was published, the definition, instance, revision and digest all
    bound, the audience was admitted, and every resolved action was declared,
    correctly typed, registered, and eligible. The validator never accepts on a
    partial match. -/
theorem accepted_implies_every_binding_matches {Ctrl Opt Aud}
    {o : Offer Ctrl Opt Aud} {r : Response Ctrl Opt Aud} {acts : List (Ctrl × Opt)}
    (h : validate o r = some acts) :
    isPublished o.state = true
      ∧ o.definitionMatches = true ∧ o.instanceMatches = true
      ∧ o.revisionMatches = true ∧ o.digestMatches = true
      ∧ o.admits r.audience = true
      ∧ o.requiredAnswered r.values = true
      ∧ ∀ s ∈ acts, o.declares s.1 = true ∧ o.typeMatches s.1 s.2 = true
                      ∧ o.registered s.2 = true ∧ o.eligible s.2 r.audience = true := by
  obtain ⟨hb, hall, hreq, hacts⟩ := accepted_inv h
  unfold bindingsOk at hb
  simp only [Bool.and_eq_true] at hb
  refine ⟨hb.1.1.1.1.1, hb.1.1.1.1.2, hb.1.1.1.2, hb.1.1.2, hb.1.2, hb.2, hreq, ?_⟩
  intro s hs
  subst hacts
  have := List.all_eq_true.mp hall s hs
  unfold submissionOk at this
  simp only [Bool.and_eq_true] at this
  exact ⟨this.1.1.1, this.1.1.2, this.1.2, this.2⟩

/-- `BHV-INTERACTION-001`: validation is ATOMIC. If any single submission
    fails its control check, nothing is accepted — there is no partial
    application. This is what keeps the soundness statement above honest for a
    multi-control response. -/
theorem validation_is_atomic {Ctrl Opt Aud}
    (o : Offer Ctrl Opt Aud) (r : Response Ctrl Opt Aud) {s : Ctrl × Opt}
    (hs : s ∈ r.values) (hbad : submissionOk o r.audience s = false) :
    validate o r = none := by
  cases hv : validate o r with
  | none => rfl
  | some acts =>
    exfalso
    obtain ⟨_, hall, _, _⟩ := accepted_inv hv
    have := List.all_eq_true.mp hall s hs
    rw [hbad] at this
    exact Bool.noConfusion this

/-- `BHV-INTERACTION-002`: an action absent from the caller's registered set
    resolves to no handler — the A3-shaped analogue of `hidden_action_rejected`.
    A response carrying one is refused outright. -/
theorem unregistered_action_never_resolves {Ctrl Opt Aud}
    (o : Offer Ctrl Opt Aud) (r : Response Ctrl Opt Aud) {s : Ctrl × Opt}
    (hs : s ∈ r.values) (hunreg : o.registered s.2 = false) :
    validate o r = none := by
  refine validation_is_atomic o r hs ?_
  unfold submissionOk
  simp [hunreg]

/-- `BHV-INTERACTION-002`: an action the responder's audience is not eligible
    for is refused EVEN WHEN it is declared, correctly typed, and registered —
    i.e. even when the surface displayed it. Eligibility is a separate fence
    from display. -/
theorem ineligible_audience_rejected {Ctrl Opt Aud}
    (o : Offer Ctrl Opt Aud) (r : Response Ctrl Opt Aud) {s : Ctrl × Opt}
    (hs : s ∈ r.values)
    (hdecl : o.declares s.1 = true) (htype : o.typeMatches s.1 s.2 = true)
    (hreg : o.registered s.2 = true)
    (hinelig : o.eligible s.2 r.audience = false) :
    validate o r = none := by
  refine validation_is_atomic o r hs ?_
  unfold submissionOk
  simp [hdecl, htype, hreg, hinelig]

/-- `BHV-INTERACTION-003`: an expired offer authorizes nothing — and, because
    the statement quantifies over EVERY response, it synthesizes nothing
    either. There is no response, however well formed, that an expired offer
    accepts (the #1837 finding: expiry must not manufacture a decision). -/
theorem expiry_never_authorizes {Ctrl Opt Aud}
    (o : Offer Ctrl Opt Aud) (hexp : o.state = State.expired) :
    ∀ r : Response Ctrl Opt Aud, validate o r = none := by
  intro r
  unfold validate bindingsOk
  simp [hexp, isPublished]

/-- The same, for every non-published state at once: `Draft`, `Answered`,
    `Cancelled`, `Expired` and `Unsupported` all refuse every response. -/
theorem unpublished_never_authorizes {Ctrl Opt Aud}
    (o : Offer Ctrl Opt Aud) (hclosed : isPublished o.state = false) :
    ∀ r : Response Ctrl Opt Aud, validate o r = none := by
  intro r
  unfold validate bindingsOk
  simp [hclosed]

/-! ### Non-vacuity

    Every theorem above is a statement about what acceptance IMPLIES, or about
    when validation REFUSES. Both families are vacuously true of a validator
    that never accepts anything, so the model needs a witness that acceptance is
    actually reachable — the Lean counterpart of `PromptControls.cfg`'s
    undisplayed decoy. `openOffer` is a fully-matching offer and
    `goodResponse` answers it; `acceptance_is_reachable` shows it goes through.
    Break any one field of `openOffer` and that witness fails. -/

/-- A fully-matching offer over `(Nat, Nat, Nat)`: control `0`, option `0`,
    audience `0`. -/
def openOffer : Offer Nat Nat Nat where
  state := State.published
  definitionMatches := true
  instanceMatches := true
  revisionMatches := true
  digestMatches := true
  declares c := c == 0
  typeMatches c v := c == 0 && v == 0
  admits a := a == 0
  registered v := v == 0
  eligible v a := v == 0 && a == 0
  requiredAnswered _ := true

/-- A response that answers `openOffer`. -/
def goodResponse : Response Nat Nat Nat where
  audience := 0
  values := [(0, 0)]

/-- Non-vacuity: this validator does accept something. -/
theorem acceptance_is_reachable : validate openOffer goodResponse = some [(0, 0)] := by
  decide

/-- ...and the decoy has teeth: audience `1` is not admitted, so the SAME
    submission from an ineligible responder is refused. -/
theorem decoy_audience_is_refused :
    validate openOffer { audience := 1, values := [(0, 0)] } = none := by
  decide

/-- ...and an unregistered option (`1`) is refused even at the good audience. -/
theorem decoy_unregistered_option_is_refused :
    validate openOffer { audience := 0, values := [(0, 1)] } = none := by
  decide

end Binding
end NewtInteraction
