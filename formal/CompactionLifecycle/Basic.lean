/- #1528 B3 — the PROACTIVE compaction LIFECYCLE kernel.

   An abstract state machine for the Responses pre-dispatch guard:
     estimate → compact → rebuild → validate → (dispatch | abort)
   `lake build` machine-checks the three B3 obligations:

   1. NO OVER-BUDGET DISPATCH — a `dispatch` state is only ever entered with the
      (post-framing) estimate within budget. So the loop never sends a request it
      already knows is too large.
   2. TERMINATION / BOUNDED NO-PROGRESS — every non-terminal step either reaches a
      terminal (`dispatch`/`abort`) or STRICTLY decreases a Nat measure; retries
      consume `fuel`, and an exhausted `validate` aborts. So there is no infinite
      compact→still-too-big→compact spin.
   3. SAME-ROUND RETRY — only a `dispatch` advances the logical `round`; every
      compaction/validation step preserves it (the B2 P1 lesson: a compaction retry
      must not consume a tool-capable round).

   The `validate` phase's provenance check (untrusted-derived material can never
   gain operator/model authority) is the B2 kernel in `CompactionProvenance.Basic`;
   this kernel models the BUDGET + ROUND + TERMINATION half of the same guard. No
   Mathlib — builds from a bare toolchain, sorry-free. Mirrors the Rust
   `compact_responses_input` + the two proactive guards in
   `newt-core/src/agentic/mod.rs`. -/
namespace NewtPolicy.CompactionLifecycle

/-- The phase of one pre-dispatch compaction decision. -/
inductive Phase where
  | estimate
  | compact
  | rebuild
  | validate
  | dispatch
  | abort
  deriving DecidableEq, Repr

/-- The abstract lifecycle state. `est` is the (calibrated real-token) request
    estimate; `budget` the actionable input budget; `framing` the fence overhead
    the rebuild adds; `fuel` the remaining retry allowance (0 = single-shot, as the
    Rust guard runs today); `round` the logical tool round. -/
structure State where
  phase : Phase
  round : Nat
  fuel : Nat
  est : Nat
  framing : Nat
  budget : Nat
  deriving Repr

/-- A terminal phase: the decision is resolved (send, or refuse). -/
def terminal (s : State) : Prop :=
  s.phase = Phase.dispatch ∨ s.phase = Phase.abort

/-- One transition. `compact` shrinks the estimate; `rebuild` re-adds the framing
    the compressor could not see (B2 BHV-BUDGET-004); `validate` is the SOLE
    in-edge to `dispatch`, and only when `est ≤ budget`; a retry consumes `fuel`,
    and `fuel = 0` aborts rather than spinning. `dispatch` advances the round. -/
def step (s : State) : State :=
  match s.phase with
  | .estimate =>
      if s.est ≤ s.budget then { s with phase := .dispatch }
      else { s with phase := .compact }
  | .compact => { s with phase := .rebuild, est := s.est / 2 }
  | .rebuild => { s with phase := .validate, est := s.est + s.framing }
  | .validate =>
      if s.est ≤ s.budget then { s with phase := .dispatch }
      else if s.fuel = 0 then { s with phase := .abort }
      else { s with phase := .compact, fuel := s.fuel - 1 }
  | .dispatch => { s with phase := .estimate, round := s.round + 1 }
  | .abort => s

/-- Rank for the termination measure: strictly higher earlier in the
    compact→rebuild→validate chain, so a step down the chain decreases it. -/
def rank : Phase → Nat
  | .estimate => 3
  | .compact => 2
  | .rebuild => 1
  | .validate => 0
  | .dispatch => 0
  | .abort => 0

/-- The well-founded measure: `fuel` dominates (each retry consumes one), tie-broken
    by the phase rank down the chain. -/
def measure (s : State) : Nat := 3 * s.fuel + rank s.phase

/-! ### Obligation 1 — no over-budget dispatch. -/

/-- A `dispatch` state is only ever produced with the estimate within budget:
    `dispatch` is reachable ONLY from `estimate`/`validate`, each guarded by
    `est ≤ budget`, and neither guard-taking step changes `est` or `budget`. -/
theorem dispatch_within_budget (s : State) (h : (step s).phase = Phase.dispatch) :
    (step s).est ≤ (step s).budget := by
  simp only [step] at h ⊢
  split at h <;> (try split at h) <;> (try split at h) <;> simp_all

/-! ### Obligation 2 — termination / bounded no-progress. -/

/-- `fuel` never increases — only a `validate` retry consumes it. -/
theorem fuel_non_increasing (s : State) : (step s).fuel ≤ s.fuel := by
  simp only [step]
  repeat' split
  all_goals simp_all
  all_goals omega

/-- An exhausted `validate` (no fuel, still over budget) ABORTS — it never spins
    back into `compact`. This is the base case that bounds the retry loop. -/
theorem validate_exhausted_aborts (s : State)
    (hp : s.phase = Phase.validate) (hf : s.fuel = 0) (hb : ¬ s.est ≤ s.budget) :
    (step s).phase = Phase.abort := by
  simp only [step, hp, hf, hb, if_false, if_true]

/-- PROGRESS: every non-terminal step either reaches a terminal phase OR strictly
    decreases the measure. With `fuel` bounding the retries, the machine therefore
    reaches `dispatch` or `abort` in finitely many steps — no infinite compaction
    spin. -/
theorem progress (s : State) (h : ¬ terminal s) :
    terminal (step s) ∨ measure (step s) < measure s := by
  simp only [terminal, step, measure, rank] at h ⊢
  repeat' split
  all_goals simp_all
  all_goals omega

/-! ### Obligation 3 — same-round retry. -/

/-- Only a `dispatch` advances the logical round. -/
theorem round_preserved_off_dispatch (s : State) (h : s.phase ≠ Phase.dispatch) :
    (step s).round = s.round := by
  simp only [step]
  repeat' split
  all_goals simp_all

/-- A completed `dispatch` advances the round by exactly one. -/
theorem dispatch_advances_round (s : State) (h : s.phase = Phase.dispatch) :
    (step s).round = s.round + 1 := by
  simp only [step, h]

/-- The round is monotone — a compaction retry never rewinds it. -/
theorem round_monotone (s : State) : s.round ≤ (step s).round := by
  simp only [step]
  repeat' split
  all_goals simp_all
  all_goals omega

end NewtPolicy.CompactionLifecycle
