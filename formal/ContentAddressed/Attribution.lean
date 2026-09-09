/- **Attributing a fabrication to where it entered the record.**

   A hallucination is not a root. It is an effect, produced at some middle step,
   and the point of keeping provenance is to walk it back to where the offending
   referent actually entered the record.

   Doing that splits three bug classes that are indistinguishable today, because
   today a phantom reach is recorded as a bare sighting — the name, how the
   harness resolved it, and nothing about what was on the wire when it happened.

     INVENTED      the name occurs nowhere in the ancestry. The model produced
                   it from nothing available to it.

     CONTAMINATED  the name's first occurrence is a WITNESSED entry — a pasted
                   document, an operator prompt, an older tool list. The model
                   read it somewhere real and reasonably assumed it was
                   available. A context-hygiene bug, not a model bug.

     SELF-INFLICTED  the name's first occurrence is a GENERATED entry — our own
                   summarizer produced a referent that never existed, and the
                   model believed us. This is our bug, it is fixable, and today
                   it is invisible: it looks exactly like the model hallucinating.

   The three need opposite fixes, which is the whole reason to tell them apart.

   ## What is proved here

     introduction_is_the_earliest_occurrence   the introducer really is first
     never_mentioned_means_unavailable         absence is decidable, not assumed
     harness_introduced_it                     the SELF-INFLICTED verdict
     witnessed_introduced_it                   the CONTAMINATED verdict
     classification_is_total                   every reach lands in exactly one

   ## What is NOT proved, and cannot be

   **Availability is not causation.** Everything here is a claim about what was
   reachable in the record, never about why the model emitted what it emitted.
   That a name was present in an ancestor does not establish that the model read
   it, and that a name was absent does not establish the model reasoned rather
   than guessed. The model's internal reason is not observable, and asserting
   one from a trace would itself be a fabrication — the exact failure this whole
   line of work exists to avoid.

   What survives is still worth having, because it is decidable: the name WAS or
   WAS NOT available, and if it was, the entry that introduced it either was or
   was not something we generated ourselves. That fact alone separates our bugs
   from the model's. -/
namespace AgentFrame.Attribution

/-- Where an entry came from. The two-way split is the one that matters for
    blame: a witnessed entry records something that happened outside the model,
    a generated entry is one we produced. Model output is never witnessed —
    a model's assertion is a claim ABOUT an event, not the event. -/
inductive Provenance where
  /-- An operator prompt, a user action, a harness event. -/
  | witnessed
  /-- Produced by a model or by our own summarizer: the fabrication surface. -/
  | generated
  deriving DecidableEq, Repr

abbrev Name := Nat

/-- One entry in the trace, reduced to what attribution needs: where it came
    from, and which referents occur in it. -/
structure Entry where
  provenance : Provenance
  names : List Name
  deriving Repr

/-- The trace preceding an emission, oldest first. -/
abbrev Trace := List Entry

/-- The first entry mentioning `n`, i.e. the one that introduced it. -/
def introducedBy (t : Trace) (n : Name) : Option Entry :=
  match t with
  | [] => none
  | e :: rest => if e.names.contains n then some e else introducedBy rest n

/-! ## Absence is decidable -/

/-- **A name that was never mentioned was never available.** Not an assumption
    about the model — a checkable fact about the record. This is what licenses
    the INVENTED verdict. -/
theorem never_mentioned_means_unavailable (t : Trace) (n : Name)
    (h : introducedBy t n = none) :
    ∀ e ∈ t, e.names.contains n = false := by
  induction t with
  | nil => intro e he; cases he
  | cons a rest ih =>
    intro e he
    unfold introducedBy at h
    split at h
    · exact absurd h (by simp)
    · rename_i hne
      cases he with
      | head => simpa using hne
      | tail _ hmem => exact ih h e hmem

/-! ## The introducer really is the earliest occurrence -/

/-- **The entry blamed for introducing `n` does contain `n`, and nothing before
    it does.** Without this the verdicts below would be blaming an arbitrary
    entry rather than the origin. -/
theorem introduction_is_the_earliest_occurrence (t : Trace) (n : Name) (e : Entry)
    (h : introducedBy t n = some e) :
    e.names.contains n = true ∧
      ∃ pre post, t = pre ++ e :: post ∧ ∀ x ∈ pre, x.names.contains n = false := by
  induction t with
  | nil => exact absurd h (by simp [introducedBy])
  | cons a rest ih =>
    unfold introducedBy at h
    split at h
    · rename_i hyes
      have hae : a = e := by simpa using h
      subst hae
      refine ⟨hyes, [], rest, by simp, ?_⟩
      intro x hx; cases hx
    · rename_i hno
      obtain ⟨hcontains, pre, post, hsplit, hclean⟩ := ih h
      refine ⟨hcontains, a :: pre, post, by simp [hsplit], ?_⟩
      intro x hx
      cases hx with
      | head => simpa using hno
      | tail _ hmem => exact hclean x hmem

/-! ## The verdicts -/

/-- **SELF-INFLICTED.** The referent entered the record through an entry WE
    generated, and nothing before it mentioned the name. Our summarizer invented
    a thing that does not exist and the model took it at face value.

    Today this is indistinguishable from the model hallucinating, because the
    reach is recorded with no edge back to the context that carried the name. -/
theorem harness_introduced_it (t : Trace) (n : Name) (e : Entry)
    (h : introducedBy t n = some e) (hg : e.provenance = Provenance.generated) :
    e.names.contains n = true ∧
      e.provenance = Provenance.generated ∧
      ∃ pre post, t = pre ++ e :: post ∧ ∀ x ∈ pre, x.names.contains n = false := by
  obtain ⟨hc, split⟩ := introduction_is_the_earliest_occurrence t n e h
  exact ⟨hc, hg, split⟩

/-- **CONTAMINATED.** The referent entered through something witnessed — a
    paste, a prompt, a stale tool list. The model read a real thing and drew a
    reasonable conclusion. Fixing this means cleaning what enters the context,
    not changing the model or the prompt. -/
theorem witnessed_introduced_it (t : Trace) (n : Name) (e : Entry)
    (h : introducedBy t n = some e) (hw : e.provenance = Provenance.witnessed) :
    e.names.contains n = true ∧
      e.provenance = Provenance.witnessed ∧
      ∃ pre post, t = pre ++ e :: post ∧ ∀ x ∈ pre, x.names.contains n = false := by
  obtain ⟨hc, split⟩ := introduction_is_the_earliest_occurrence t n e h
  exact ⟨hc, hw, split⟩

/-- The verdict a trace assigns to an emitted name. -/
inductive Verdict where
  /-- Occurs nowhere in the ancestry. -/
  | invented
  /-- First occurs in a witnessed entry: it came from outside. -/
  | contaminated
  /-- First occurs in a generated entry: we made it up. -/
  | selfInflicted
  deriving DecidableEq, Repr

def classify (t : Trace) (n : Name) : Verdict :=
  match introducedBy t n with
  | none => Verdict.invented
  | some e =>
    match e.provenance with
    | Provenance.witnessed => Verdict.contaminated
    | Provenance.generated => Verdict.selfInflicted

/-- **Every reach lands in exactly one class**, computably, from the trace
    alone. No model introspection, no heuristic, no scoring — which is what
    makes this a diagnostic rather than another guess. -/
theorem classification_is_total (t : Trace) (n : Name) :
    classify t n = Verdict.invented ∨
    classify t n = Verdict.contaminated ∨
    classify t n = Verdict.selfInflicted := by
  unfold classify
  split
  · exact Or.inl rfl
  · rename_i e _
    cases e.provenance
    · exact Or.inr (Or.inl rfl)
    · exact Or.inr (Or.inr rfl)

/-- **`invented` means what it says.** The verdict is not a default reached by
    giving up on the search; it is entailed by the name being absent from every
    entry in the trace. -/
theorem invented_iff_absent (t : Trace) (n : Name) :
    classify t n = Verdict.invented → ∀ e ∈ t, e.names.contains n = false := by
  intro h
  unfold classify at h
  split at h
  · rename_i hnone; exact never_mentioned_means_unavailable t n hnone
  · rename_i e _
    cases hp : e.provenance <;> rw [hp] at h <;> exact absurd h (by simp)

end AgentFrame.Attribution
