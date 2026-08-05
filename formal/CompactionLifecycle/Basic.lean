/- #1528 B3 — the PROACTIVE compaction LIFECYCLE kernel.

   An abstract state machine for the Responses pre-dispatch guard:
     estimate → compact → rebuild → validate → (readyToDispatch | abort)
   `lake build` machine-checks the B3 obligations:

   1. NO OVER-BUDGET DISPATCH (REACHABLE safety) — EVERY state REACHABLE from an
      initial `estimate` state that is `readyToDispatch` has `est ≤ budget`. So the
      loop never reaches "safe to send" with an oversized request.
   2. ACTUAL FINITE TERMINATION — `eventually_terminal`: from any state, iterating
      `step` reaches a terminal (`readyToDispatch` | `abort`) in finitely many
      steps. Proven by well-founded recursion on a strictly-decreasing measure;
      retries consume `fuel`, and an exhausted `validate` aborts.
   3. SAME-ROUND / round ≠ safe-to-send — NO lifecycle step advances the logical
      `round`; reaching `readyToDispatch` is NOT a completed round. Round
      advancement on a COMPLETED dispatch is the wider agent-turn / recovery model's
      obligation, deliberately OUT of this kernel (Problem C, Preferred shape).

   The `validate` phase's PROVENANCE check (untrusted-derived material never gains
   operator/model authority) is the B2 kernel in `CompactionProvenance.Basic`; this
   kernel models the BUDGET + ROUND + TERMINATION half. Mirrors the Rust
   `compact_responses_input` + the proactive guards in
   `newt-core/src/agentic/mod.rs`. No Mathlib — bare toolchain, sorry-free. -/
namespace NewtPolicy.CompactionLifecycle

/-- The phase of one pre-dispatch compaction decision. `readyToDispatch` (safe to
    send) and `abort` (fail closed) are TERMINAL for this B3 kernel — the actual
    network dispatch, its completion, and any resulting `round` advancement are
    modelled elsewhere (Problem C). -/
inductive Phase where
  | estimate
  | compact
  | rebuild
  | validate
  | readyToDispatch
  | abort
  deriving DecidableEq, Repr

/-- The abstract lifecycle state. `est` is the (calibrated real-token) request
    estimate; `budget` the actionable input budget; `framing` the fence overhead the
    rebuild adds; `fuel` the remaining retry allowance (0 = single-shot, as the Rust
    guard runs today); `round` the logical tool round. -/
structure State where
  phase : Phase
  round : Nat
  fuel : Nat
  est : Nat
  framing : Nat
  budget : Nat
  deriving Repr

/-- A terminal phase: the pre-dispatch decision is resolved (safe to send, or
    refuse). -/
def terminal (s : State) : Prop :=
  s.phase = Phase.readyToDispatch ∨ s.phase = Phase.abort

/-- One transition. `compact` shrinks the estimate; `rebuild` re-adds the framing
    the compressor could not see (B2 BHV-BUDGET-004); `validate` and `estimate` are
    the ONLY in-edges to `readyToDispatch`, each guarded by `est ≤ budget`; a retry
    consumes `fuel`, and `fuel = 0` ABORTS rather than spinning. NO step touches
    `round` — reaching `readyToDispatch` is not a completed round. -/
def step (s : State) : State :=
  match s.phase with
  | .estimate =>
      if s.est ≤ s.budget then { s with phase := .readyToDispatch }
      else { s with phase := .compact }
  | .compact => { s with phase := .rebuild, est := s.est / 2 }
  | .rebuild => { s with phase := .validate, est := s.est + s.framing }
  | .validate =>
      if s.est ≤ s.budget then { s with phase := .readyToDispatch }
      else if s.fuel = 0 then { s with phase := .abort }
      else { s with phase := .compact, fuel := s.fuel - 1 }
  | .readyToDispatch => s
  | .abort => s

/-- Iterate `step` `n` times (apply-first, so `run (n+1) s = run n (step s)`). -/
def run : Nat → State → State
  | 0, s => s
  | n + 1, s => run n (step s)

/-- Rank for the termination measure: strictly higher earlier in the
    compact→rebuild→validate chain. -/
def rank : Phase → Nat
  | .estimate => 3
  | .compact => 2
  | .rebuild => 1
  | .validate => 0
  | .readyToDispatch => 0
  | .abort => 0

/-- The well-founded measure: `fuel` dominates (each retry consumes one), tie-broken
    by the phase rank down the chain. -/
def measure (s : State) : Nat := 3 * s.fuel + rank s.phase

/-! ### Obligation 1 — reachable no-over-budget dispatch. -/

/-- The reachability invariant: a `readyToDispatch` state is within budget. -/
def Inv (s : State) : Prop := s.phase = Phase.readyToDispatch → s.est ≤ s.budget

/-- `Inv` is PRESERVED by `step` (given `Inv s`): a step ENTERS `readyToDispatch`
    only through the `est ≤ budget` guard (from `estimate`/`validate`), and the
    `readyToDispatch` self-loop carries its already-established `est ≤ budget`
    forward via `hs`. -/
theorem step_preserves_inv (s : State) (hs : Inv s) : Inv (step s) := by
  intro hd
  cases hp : s.phase
  case estimate =>
    by_cases hb : s.est ≤ s.budget
    · simp only [step, hp, if_pos hb]; exact hb
    · simp only [step, hp, if_neg hb] at hd; exact absurd hd (by decide)
  case compact => simp only [step, hp] at hd; exact absurd hd (by decide)
  case rebuild => simp only [step, hp] at hd; exact absurd hd (by decide)
  case validate =>
    by_cases hb : s.est ≤ s.budget
    · simp only [step, hp, if_pos hb]; exact hb
    · by_cases hf : s.fuel = 0
      · simp only [step, hp, if_neg hb, if_pos hf] at hd; exact absurd hd (by decide)
      · simp only [step, hp, if_neg hb, if_neg hf] at hd; exact absurd hd (by decide)
  case readyToDispatch => simp only [step, hp]; exact hs hp
  case abort => simp only [step, hp] at hd; exact absurd hd (by decide)

/-- An initial state (`estimate`) trivially satisfies `Inv`. -/
def Initial (s : State) : Prop := s.phase = Phase.estimate

theorem initial_inv (s : State) (h : Initial s) : Inv s := by
  intro hd; rw [Initial] at h; rw [h] at hd; exact absurd hd (by decide)

/-- Reflexive-transitive reachability under `step` from a fixed `init`. -/
inductive Reachable (init : State) : State → Prop
  | refl : Reachable init init
  | step {t : State} : Reachable init t → Reachable init (step t)

/-- Every reachable state satisfies `Inv`. -/
theorem reachable_inv {init cur : State} (hi : Initial init) (hr : Reachable init cur) :
    Inv cur := by
  induction hr with
  | refl => exact initial_inv init hi
  | step _ ih => exact step_preserves_inv _ ih

/-- OBLIGATION 1: every state REACHABLE from an initial `estimate` state that is
    `readyToDispatch` has `est ≤ budget`. The loop never reaches "safe to send"
    with an oversized request. -/
theorem reachable_ready_to_dispatch_within_budget {init cur : State}
    (hi : Initial init) (hr : Reachable init cur) (hd : cur.phase = Phase.readyToDispatch) :
    cur.est ≤ cur.budget :=
  reachable_inv hi hr hd

/-! ### Obligation 2 — actual finite termination. -/

/-- `fuel` never increases — only a `validate` retry consumes it. -/
theorem fuel_never_increases (s : State) : (step s).fuel ≤ s.fuel := by
  simp only [step]
  repeat' split
  all_goals simp_all
  all_goals omega

/-- An exhausted `validate` (no fuel, still over budget) ABORTS — never spins back
    into `compact`. The base case that bounds the retry loop. -/
theorem exhausted_validation_aborts (s : State)
    (hp : s.phase = Phase.validate) (hf : s.fuel = 0) (hb : ¬ s.est ≤ s.budget) :
    (step s).phase = Phase.abort := by
  simp [step, hp, hf, hb]

/-- PROGRESS: every non-terminal step either reaches a terminal phase OR strictly
    decreases the measure. -/
theorem progress (s : State) (h : ¬ terminal s) :
    terminal (step s) ∨ measure (step s) < measure s := by
  simp only [terminal, step, measure, rank] at h ⊢
  repeat' split
  all_goals simp_all
  all_goals omega

/-- OBLIGATION 2: from ANY state, iterating `step` reaches a terminal
    (`readyToDispatch` | `abort`) in finitely many steps — no infinite
    compact→still-too-big→compact spin. Proven by well-founded recursion on the
    strictly-decreasing `measure`. -/
theorem eventually_terminal (s : State) : ∃ n, terminal (run n s) := by
  by_cases ht : terminal s
  · exact ⟨0, ht⟩
  · rcases progress s ht with hterm | hlt
    · exact ⟨1, hterm⟩
    · obtain ⟨n, hn⟩ := eventually_terminal (step s)
      exact ⟨n + 1, hn⟩
  termination_by measure s
  decreasing_by exact hlt

/-- Named corollary matching the acceptance contract. -/
theorem eventually_ready_or_abort (s : State) :
    ∃ n, (run n s).phase = Phase.readyToDispatch ∨ (run n s).phase = Phase.abort :=
  eventually_terminal s

/-! ### Obligation 3 — round is preserved (safe-to-send ≠ completed round). -/

/-- NO lifecycle step advances the logical `round`: reaching `readyToDispatch` is
    NOT a completed round. (Round advancement on a COMPLETED dispatch belongs to the
    wider agent-turn model — deliberately out of this kernel, Problem C.) -/
theorem no_lifecycle_step_advances_round (s : State) : (step s).round = s.round := by
  simp only [step]
  repeat' split
  all_goals simp_all

/-- Per-phase corollaries (the acceptance-contract names). -/
theorem compaction_preserves_round (s : State) : (step s).round = s.round :=
  no_lifecycle_step_advances_round s
theorem rebuild_preserves_round (s : State) : (step s).round = s.round :=
  no_lifecycle_step_advances_round s
theorem validation_preserves_round (s : State) : (step s).round = s.round :=
  no_lifecycle_step_advances_round s
theorem abort_preserves_round (s : State) : (step s).round = s.round :=
  no_lifecycle_step_advances_round s
theorem ready_to_dispatch_preserves_round (s : State) : (step s).round = s.round :=
  no_lifecycle_step_advances_round s

/-! ### Non-vacuity demonstrations.

    Each `example` pins a load-bearing guard: it holds ONLY because the guard is
    present, so a regression that removes the guard turns the `rfl`/proof into a
    build failure (the model REJECTS the historical defect class, not merely
    compiles). -/

/-- If the `est ≤ budget` guard were dropped from the `estimate` edge (straight to
    `readyToDispatch`), an over-budget state would reach "safe to send" and break
    `reachable_ready_to_dispatch_within_budget`. It instead routes to `compact`. -/
example :
    (step { phase := .estimate, round := 7, fuel := 1, est := 100, framing := 0, budget := 10 }).phase
      = Phase.compact := rfl

/-- If exhausted `validate` RETRIED instead of aborting, `eventually_terminal` would
    break (an infinite compaction spin). It instead aborts. -/
example :
    (step { phase := .validate, round := 7, fuel := 0, est := 100, framing := 0, budget := 10 }).phase
      = Phase.abort := rfl

/-- If any lifecycle step advanced `round`, `no_lifecycle_step_advances_round` would
    break — reaching `readyToDispatch` must NOT be counted as a completed round. -/
example :
    (step { phase := .validate, round := 7, fuel := 3, est := 5, framing := 0, budget := 10 }).round
      = 7 := rfl

end NewtPolicy.CompactionLifecycle
