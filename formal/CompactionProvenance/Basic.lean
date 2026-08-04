/- #1528 B2 — the compaction-bridge PROVENANCE kernel.

   A pure classification + authority-mapping policy. `lake build` machine-checks
   that untrusted-derived provenance can NEVER acquire operator/model authority,
   and that an unknown wire role can never be classified as operator/assistant.

   Mirrors the Rust `CompactionProvenance` / `WireAuthority` in
   `newt-core/src/agentic/responses_compaction.rs`; the Rust test
   `rust_authority_matches_the_lean_oracle` differentially checks the two agree
   over the whole (finite) provenance set. No Mathlib — builds from a bare
   toolchain. -/
namespace NewtPolicy.CompactionProvenance

/-- The trust provenance of one message crossing the compaction bridge. -/
inductive Provenance where
  | operatorUser
  | assistant
  | internalSummary
  | toolOutput
  | opaqueUntrusted
  deriving DecidableEq, Repr

/-- The authority a rebuilt wire item carries. -/
inductive WireAuthority where
  | operator
  | model
  | reference
  | untrusted
  deriving DecidableEq, Repr

/-- A classification that fails CLOSED rather than assign a trusted role. -/
inductive BridgeError where
  | unknownRole
  | malformedItem
  deriving DecidableEq, Repr

/-- The authority a provenance rebuilds to. Total; the only trusted results
    (`operator`/`model`) come from `operatorUser`/`assistant`. -/
def rebuildAuthority : Provenance → Except BridgeError WireAuthority
  | .operatorUser    => .ok .operator
  | .assistant       => .ok .model
  | .internalSummary => .ok .reference
  | .toolOutput      => .ok .untrusted
  | .opaqueUntrusted => .ok .untrusted

theorem tool_output_never_operator :
    rebuildAuthority .toolOutput ≠ .ok .operator := by simp [rebuildAuthority]

theorem summary_never_operator :
    rebuildAuthority .internalSummary ≠ .ok .operator := by simp [rebuildAuthority]

theorem opaque_never_trusted :
    rebuildAuthority .opaqueUntrusted ≠ .ok .operator ∧
    rebuildAuthority .opaqueUntrusted ≠ .ok .model := by simp [rebuildAuthority]

theorem trusted_classes_are_preserved :
    rebuildAuthority .operatorUser = .ok .operator ∧
    rebuildAuthority .assistant = .ok .model := by simp [rebuildAuthority]

/-- Classify a raw wire role, failing CLOSED. Mirrors the Rust
    `responses_input_to_compaction` role match: `system` inside `input` is
    rejected, and an UNKNOWN role becomes opaque-untrusted — never a trusted role. -/
def classifyRole : String → Except BridgeError Provenance
  | "user"      => .ok .operatorUser
  | "assistant" => .ok .assistant
  | "tool"      => .ok .opaqueUntrusted   -- an untrusted tool result
  | "system"    => .error .unknownRole    -- forbidden inside `input`
  | _           => .ok .opaqueUntrusted   -- unknown → fail closed, never trusted

/-- A trusted operator classification comes ONLY from the exact "user" role —
    so an unknown/malformed role can never be promoted to operator authority. -/
theorem operator_only_from_user (r : String) :
    classifyRole r = .ok .operatorUser → r = "user" := by
  intro h
  unfold classifyRole at h
  split at h <;> simp_all

/-- A trusted model classification comes ONLY from the exact "assistant" role. -/
theorem assistant_only_from_assistant (r : String) :
    classifyRole r = .ok .assistant → r = "assistant" := by
  intro h
  unfold classifyRole at h
  split at h <;> simp_all

end NewtPolicy.CompactionProvenance
