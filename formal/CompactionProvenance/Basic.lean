/- #1528 B2 — the compaction-bridge PROVENANCE kernel.

   A pure classification + authority-mapping policy. `lake build` machine-checks
   that untrusted-derived provenance can NEVER acquire operator/model authority,
   that an unknown wire role can never be classified as operator/assistant, and
   (the B2-v2 role-gate) that a `tool` source is ALWAYS `toolOutput` regardless of
   any in-band marker it carries — so tool-controlled text cannot spoof a harness
   summary or a validated envelope.

   Mirrors the Rust `CompactionProvenance` / `WireAuthority` and the role-gated
   classifier in `newt-core/src/agentic/responses_compaction.rs`; the Rust test
   `rust_authority_mirrors_the_lean_model_table` differentially checks the two
   agree over the whole (finite) provenance set. No Mathlib — builds from a bare
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

/-! ## The role-gated classifier (B2-v2)

    The Rust bridge classifies a message by its STRUCTURAL role FIRST, then (only
    for a `user` role) by a strict-parsed durable marker. This section models that
    exact gate and proves the security-relevant closure properties. -/

/-- The structural wire role of a message the bridge classifies. -/
inductive SourceRole where
  | user
  | assistant
  | tool
  | system
  | unknown
  deriving DecidableEq, Repr

/-- The durable marker recognized on a message body. `summaryPrefix` is the
    compressor's own bare summary; `summaryEnvelope` / `untrustedEnvelope` are
    STRICT-parsed re-fed Newt envelopes; `reservedMalformed` begins with a reserved
    prefix but did not fully parse (fail closed); `plain` is ordinary content. -/
inductive Marker where
  | plain
  | summaryPrefix
  | summaryEnvelope
  | untrustedEnvelope
  | reservedMalformed
  deriving DecidableEq, Repr

/-- Classify `(role, marker)` into provenance, ROLE-GATED. Mirrors the Rust
    `chat_to_compaction` / `classify_user_content` gate: the `tool` arm is FIRST
    and IGNORES the marker (CG-1); a `user` durable marker is honored only after
    the role gate; `system` is a summary only with `summaryPrefix`, else fails
    closed; an `unknown` role always fails closed. No arm yields a trusted role
    except from `user`/`assistant`. -/
def classify : SourceRole → Marker → Except BridgeError Provenance
  | .tool,      _                  => .ok .toolOutput        -- CG-1: marker ignored
  | .assistant, _                  => .ok .assistant
  | .user,      .summaryPrefix     => .ok .internalSummary   -- compressor's own summary (downgrade)
  | .user,      .summaryEnvelope   => .ok .internalSummary   -- re-fed summary envelope
  | .user,      .untrustedEnvelope => .ok .opaqueUntrusted   -- re-fed untrusted envelope
  | .user,      .reservedMalformed => .ok .opaqueUntrusted   -- CG-4: fail closed
  | .user,      .plain             => .ok .operatorUser
  | .system,    .summaryPrefix     => .ok .internalSummary
  | .system,    _                  => .ok .opaqueUntrusted   -- non-summary system fails closed
  | .unknown,   _                  => .ok .opaqueUntrusted   -- fail closed, never trusted

/-- CG-1: a `tool` source is ALWAYS `toolOutput`, whatever marker its body carries
    — so tool-controlled text can never self-identify as a summary or envelope. -/
theorem tool_role_always_tool_output (m : Marker) :
    classify .tool m = .ok .toolOutput := by cases m <;> rfl

/-- Corollary: a tool result is never reclassified as a harness summary. -/
theorem tool_never_summary (m : Marker) :
    classify .tool m ≠ .ok .internalSummary := by
  rw [tool_role_always_tool_output]; simp

/-- Corollary: a tool result never acquires operator provenance. -/
theorem tool_never_operator (m : Marker) :
    classify .tool m ≠ .ok .operatorUser := by
  rw [tool_role_always_tool_output]; simp

/-- CG-4: a `user` body that begins with a reserved prefix but did not parse fails
    CLOSED — its provenance rebuilds to `untrusted`, never a trusted authority. -/
theorem malformed_reserved_never_trusted :
    (classify .user .reservedMalformed).bind rebuildAuthority = .ok .untrusted := rfl

/-- An `unknown` wire role never classifies to a trusted provenance, for any
    marker — the composite authority is always `untrusted`. -/
theorem unknown_role_never_trusted (m : Marker) :
    (classify .unknown m).bind rebuildAuthority = .ok .untrusted := by
  cases m <;> rfl

/-- The umbrella closure property: a NON-user, NON-assistant source can never reach
    operator OR model authority, regardless of marker. (tool / system / unknown.) -/
theorem untrusted_source_never_trusted_authority (r : SourceRole) (m : Marker)
    (hr : r ≠ .user) (ha : r ≠ .assistant) :
    (classify r m).bind rebuildAuthority ≠ .ok .operator ∧
    (classify r m).bind rebuildAuthority ≠ .ok .model := by
  cases r <;> cases m <;>
    simp_all [classify, rebuildAuthority, Except.bind]

/-! ## Legacy raw-role classifier (forward path)

    The Rust `responses_input_to_compaction` role match on the raw wire role.
    Retained + strengthened: `tool` now classifies to `toolOutput` (aligned with
    the B2-v2 gate above), `system` is rejected, unknown fails closed. -/

/-- Classify a raw wire role, failing CLOSED. `system` inside `input` is rejected;
    an UNKNOWN role becomes opaque-untrusted; `tool` is a tool result. -/
def classifyRole : String → Except BridgeError Provenance
  | "user"      => .ok .operatorUser
  | "assistant" => .ok .assistant
  | "tool"      => .ok .toolOutput
  | "system"    => .error .unknownRole
  | _           => .ok .opaqueUntrusted

/-- A trusted operator classification comes ONLY from the exact "user" role. -/
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
