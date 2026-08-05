/- #1528 B5 — the strict Responses WIRE-VALIDATION kernel.

   A pure model of the ONE typed gate every Responses dispatch passes through in
   the Rust (`newt-core/src/agentic/responses_wire_validation.rs` +
   `newt-core/src/agentic/mod.rs`): `validate_responses_request` turns a raw request
   into a `ValidatedResponsesRequest` — a newtype with NO public constructor other
   than a successful validation — and `dispatch_responses_json` accepts ONLY that
   newtype, so a broken request cannot compile its way to `POST /v1/responses`.

   This kernel abstracts the CONCRETE JSON/Serde/HTTP shape (covered by the Rust
   mock-server contract tests) down to the load-bearing structure:

   * A `Request` carries the wire facts the validator decides on — whether a model
     is present, whether the `store` field matches the explicit endpoint policy,
     whether the forbidden `num_ctx` is present, the number of instruction SOURCES
     (top-level field + any laundered system item — must be exactly one), the count
     of dangling function-call outputs (must be zero), the count of bad content
     handles (malformed OR foreign-session — must be zero), whether the estimate
     fits the budget, whether this is the tools-disabled final summary, and its tool
     count.
   * `isValid` is the conjunction of the fail-closed checks — the exact set the Rust
     `validate_responses_request` enforces, in the same "any one fails ⇒ reject"
     shape.
   * `ValidatedRequest` PAIRS a request with a PROOF `isValid r = true`, so a value
     of the type cannot exist for an invalid request (the Rust newtype's invariant).
   * `validate : Request → Option ValidatedRequest` returns `some` iff `isValid`, and
     `Dispatchable r` holds iff a `ValidatedRequest` exists for `r` — true ONLY for a
     validated request.

   ## What `lake build` machine-checks (the wire-gate laws)

     validated_request_has_exactly_one_instruction_source   a validated request has exactly 1 instruction source
     validated_request_has_no_num_ctx                       a validated request never carries num_ctx
     validated_final_summary_has_no_tools                   a validated final summary carries 0 tools
     validated_request_has_no_dangling_output               a validated request has 0 dangling outputs
     validation_failure_cannot_produce_validated_request    validate = none ⇒ nothing dispatchable
     only_validated_request_is_dispatchable                 Dispatchable r ⇒ validate r = some vr

   ## Deliberately NOT modelled here (Rust contract tests' domain)

   - The concrete JSON scanning (CID-marker extraction, role classification, the
     flattened tool shape, calibrated token estimation) — the mock-server tests in
     `mod.rs` and the unit tests in `responses_wire_validation.rs` cover the wire.
   - This kernel proves the STRUCTURAL guarantee: a request that fails ANY check is
     not dispatchable, and every dispatchable request passed every check.

   No Mathlib; bare toolchain; fully machine-checked with no proof holes. -/

namespace NewtPolicy.ResponsesWire

/-! ### The abstract request and the validator. -/

/-- The wire facts the validator decides a Responses request on. Each field is the
    abstraction of one concrete check in the Rust `validate_responses_request`. -/
structure Request where
  /-- `model` present and nonempty. -/
  hasModel : Bool
  /-- `store` matches the explicit endpoint policy. -/
  storeOk : Bool
  /-- Ollama's `num_ctx` is present (forbidden on this wire). -/
  hasNumCtx : Bool
  /-- Instruction SOURCES: top-level field + any laundered system input item. -/
  instructionSources : Nat
  /-- Function-call outputs with no matching preceding call. -/
  danglingOutputs : Nat
  /-- Function calls with no matching following output. -/
  danglingCalls : Nat
  /-- Content handles that are malformed OR foreign to this session. -/
  badCidMarkers : Nat
  /-- The request estimate fits the actionable budget. -/
  fitsBudget : Bool
  /-- This is the tools-disabled FINAL SUMMARY request. -/
  isFinalSummary : Bool
  /-- The number of tool schemas the request carries. -/
  toolCount : Nat
  deriving DecidableEq, Repr

/-- The fail-closed conjunction the Rust validator enforces: EVERY check must pass.
    `isFinalSummary → toolCount = 0` is the "final summary has no tools" rule. -/
def isValid (r : Request) : Bool :=
  r.hasModel && r.storeOk && (!r.hasNumCtx) &&
    (r.instructionSources == 1) && (r.danglingOutputs == 0) &&
    (r.danglingCalls == 0) && (r.badCidMarkers == 0) && r.fitsBudget &&
    (!r.isFinalSummary || r.toolCount == 0)

/-- A VALIDATED request: a request paired with a PROOF it is valid. The proof field
    is the Lean analogue of the Rust newtype having no public constructor other than
    a successful `validate_responses_request` — a value cannot exist for an invalid
    request. -/
structure ValidatedRequest where
  request : Request
  valid : isValid request = true

/-- The ONE validator: `some` exactly when `isValid`, carrying the validity proof;
    `none` otherwise. The sole way to obtain a `ValidatedRequest`. -/
def validate (r : Request) : Option ValidatedRequest :=
  if h : isValid r = true then some ⟨r, h⟩ else none

/-- A request is DISPATCHABLE iff a `ValidatedRequest` exists for it — i.e. only a
    validated request may be dispatched. -/
def Dispatchable (r : Request) : Prop := ∃ vr : ValidatedRequest, vr.request = r

/-! ### Decomposing validity: each field-check follows from `isValid = true`. -/

/-- The whole conjunction reduces field-by-field. Every headline law below is one
    projection of this flat fact-tuple. (Bare toolchain: no Mathlib `tauto`, so the
    left-nested `&&` is destructured explicitly and re-associated right.) -/
theorem valid_facts (r : Request) (h : isValid r = true) :
    r.hasModel = true ∧ r.storeOk = true ∧ r.hasNumCtx = false ∧
      r.instructionSources = 1 ∧ r.danglingOutputs = 0 ∧ r.danglingCalls = 0 ∧
      r.badCidMarkers = 0 ∧ r.fitsBudget = true ∧
      (r.isFinalSummary = false ∨ r.toolCount = 0) := by
  simp only [isValid, Bool.and_eq_true, beq_iff_eq, Bool.not_eq_true', Bool.or_eq_true] at h
  obtain ⟨⟨⟨⟨⟨⟨⟨⟨hm, hs⟩, hn⟩, hi⟩, hdo⟩, hdc⟩, hb⟩, hfb⟩, hfinal⟩ := h
  exact ⟨hm, hs, hn, hi, hdo, hdc, hb, hfb, hfinal⟩

/-! ### The six machine-checked wire-gate laws. -/

/-- (LAW) A validated request has EXACTLY ONE instruction source — never zero, never
    a top-level field duplicated by a laundered system item. -/
theorem validated_request_has_exactly_one_instruction_source (vr : ValidatedRequest) :
    vr.request.instructionSources = 1 :=
  (valid_facts vr.request vr.valid).2.2.2.1

/-- (LAW) A validated request never carries Ollama's `num_ctx`. -/
theorem validated_request_has_no_num_ctx (vr : ValidatedRequest) :
    vr.request.hasNumCtx = false :=
  (valid_facts vr.request vr.valid).2.2.1

/-- (LAW) A validated FINAL SUMMARY request carries NO tools. -/
theorem validated_final_summary_has_no_tools (vr : ValidatedRequest)
    (h : vr.request.isFinalSummary = true) : vr.request.toolCount = 0 := by
  rcases (valid_facts vr.request vr.valid).2.2.2.2.2.2.2.2 with hf | ht
  · rw [hf] at h; simp at h
  · exact ht

/-- (LAW) A validated request has NO dangling function-call output. -/
theorem validated_request_has_no_dangling_output (vr : ValidatedRequest) :
    vr.request.danglingOutputs = 0 :=
  (valid_facts vr.request vr.valid).2.2.2.2.1

/-- A ValidatedRequest exists for `r` iff `isValid r`. The bridge between the
    proof-carrying type and the boolean check. -/
theorem dispatchable_iff_valid (r : Request) : Dispatchable r ↔ isValid r = true := by
  constructor
  · rintro ⟨vr, rfl⟩; exact vr.valid
  · intro h; exact ⟨⟨r, h⟩, rfl⟩

/-- `validate` succeeds exactly when `isValid`. -/
theorem validate_isSome_iff (r : Request) : (validate r).isSome = true ↔ isValid r = true := by
  unfold validate
  by_cases h : isValid r = true <;> simp [h]

/-- (LAW) A validation FAILURE cannot produce a validated request: if `validate`
    returns `none`, nothing is dispatchable (no `ValidatedRequest` exists for `r`). -/
theorem validation_failure_cannot_produce_validated_request (r : Request)
    (h : validate r = none) : ¬ Dispatchable r := by
  rw [dispatchable_iff_valid]
  intro hv
  rw [← validate_isSome_iff] at hv
  rw [h] at hv
  simp at hv

/-- (LAW) ONLY a validated request is dispatchable: every dispatchable request is one
    `validate` accepts, and it yields a `ValidatedRequest` wrapping exactly it. -/
theorem only_validated_request_is_dispatchable (r : Request) (h : Dispatchable r) :
    ∃ vr : ValidatedRequest, validate r = some vr := by
  rw [dispatchable_iff_valid] at h
  exact ⟨⟨r, h⟩, by unfold validate; simp [h]⟩

/-! ### Non-vacuity: the laws firing on concrete requests.

    `good` passes every check; the `bad*` requests each fail exactly one, exhibiting
    that a single violated invariant makes a request undispatchable. -/

def good : Request :=
  { hasModel := true, storeOk := true, hasNumCtx := false, instructionSources := 1,
    danglingOutputs := 0, danglingCalls := 0, badCidMarkers := 0, fitsBudget := true,
    isFinalSummary := false, toolCount := 3 }

/-- The well-formed request validates. -/
example : isValid good = true := by decide

/-- A tools-disabled final summary that carries tools is rejected. -/
example : isValid { good with isFinalSummary := true } = false := by decide

/-- The same summary with zero tools validates. -/
example : isValid { good with isFinalSummary := true, toolCount := 0 } = true := by decide

/-- A request carrying `num_ctx` is rejected. -/
example : isValid { good with hasNumCtx := true } = false := by decide

/-- Zero instruction sources is rejected (not just duplication). -/
example : isValid { good with instructionSources := 0 } = false := by decide

/-- Two instruction sources (a laundered duplicate) is rejected. -/
example : isValid { good with instructionSources := 2 } = false := by decide

/-- A dangling function-call output is rejected. -/
example : isValid { good with danglingOutputs := 1 } = false := by decide

/-- A foreign / malformed content handle is rejected. -/
example : isValid { good with badCidMarkers := 1 } = false := by decide

/-- A `store` policy mismatch is rejected. -/
example : isValid { good with storeOk := false } = false := by decide

/-- An over-budget request is rejected. -/
example : isValid { good with fitsBudget := false } = false := by decide

/-- The well-formed request is dispatchable; a `num_ctx`-bearing one is not. -/
example : Dispatchable good := (dispatchable_iff_valid good).mpr (by decide)
example : ¬ Dispatchable { good with hasNumCtx := true } := by
  rw [dispatchable_iff_valid]; decide

end NewtPolicy.ResponsesWire
