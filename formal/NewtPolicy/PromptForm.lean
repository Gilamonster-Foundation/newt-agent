/-
  NewtPolicy.PromptForm — the prompt/action authorization kernel
  (`BHV-PROMPT-001`, see `spec/behavior-map.toml`).

  The unified prompt contract: a permission prompt publishes ONE serialized
  form, and only an action that form actually displays can authorize. This is
  the per-step decision kernel behind the Rust parser
  (`newt-core/src/tty/widgets/question.rs::parse`) and the store-side
  revalidation (`newt-core/src/store.rs::answer_permission_action`); the TLC
  model `spec/tla/PromptControls.tla` checks the same rule temporally
  (`AuthorizationDisplayed`, with an undisplayed decoy action in its cfg).

  Self-contained (no Mathlib), matching the rest of `NewtPolicy`.
-/

namespace NewtPolicy
namespace PromptForm

/-- A published prompt form: which actions it actually displays. -/
structure Form (Action : Type) where
  actions : Action → Bool

/-- The authorization kernel: grant an action iff the form displays it. -/
def authorize {Action} (form : Form Action) (action : Action) : Option Action :=
  if form.actions action then some action else none

/-- `action` is displayed by `form`. -/
def displayed {Action} (form : Form Action) (action : Action) : Prop :=
  form.actions action = true

/-- `BHV-PROMPT-001`: anything `authorize` grants was displayed. -/
theorem authorization_sound {Action} (form : Form Action) {requested granted : Action}
    (h : authorize form requested = some granted) : displayed form granted := by
  by_cases hs : form.actions requested = true
  · have : granted = requested := by simpa [authorize, hs] using h.symm
    subst granted
    simp [displayed, hs]
  · exfalso
    simp [authorize, hs] at h

/-- `BHV-PROMPT-001`: a hidden (undisplayed) action is rejected outright. -/
theorem hidden_action_rejected {Action} (form : Form Action) (action : Action)
    (hidden : form.actions action = false) : authorize form action = none := by
  simp [authorize, hidden]

end PromptForm
end NewtPolicy
