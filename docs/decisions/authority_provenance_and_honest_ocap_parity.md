# Decision: newt is the origin of the provenance chain, and honest OCAP parity depends on it

**Status:** accepted (2026-08-09) — operator-approved plan; implementation stacked
**Date:** 2026-08-09
**Owner:** Claude (steward); operator (Shawn) approved
**Refs:** agent-bridle `docs/adr/0025`, agent-mesh `docs/decisions/authority_fidelity_and_content_addressed_provenance.md`,
the audit + plan on the knowledge board, newt-agent#1492 (`feat/psyche`), Terminal-Bench epic #1419.

## Context

An adversarial audit of agent-bridle #317 proved its OCAP admission checks only enforcement
*strength*, never *scope* — so a confined child can be admitted with a kernel-enforced authority
**wider** than its Caveat (sh→bash, loader/rootfs reads, egress-proxy substitution). This is not
only a security defect: it directly threatens newt's headline deliverable — the Terminal-Bench
`Model | OCAP off | OCAP on` table on `README.md` and the **OCAP-off ≈ OCAP-on parity** claim. If
today's OCAP-on lane silently widens authority, its bench numbers are measured under an
**under-confined** lane, and any "parity" is dishonest (OCAP-off vs a secretly-widened OCAP-on).

## Decision

Adopt content-addressed authority provenance (7-law spine — agent-bridle ADR 0025). newt's role in
the chain:

- **newt is the origin of Intent** — the model's request (`ToolEvent::from_call_in_model_round`,
  `PermissionRequest.target` on a `DenialKind` axis) is *requested* authority, never permission
  (L: intent ≠ authority). What newt *admits* is the enforced `Caveats` (`GrantId = CID` once the
  mesh types land).
- **Harness/model separation stays key-holding, made explicit.** The model runs under session
  `Caveats`; the harness keeps its authority by holding the operator root key. `~/.newt` (OCAP
  store, permission log, **signing keys**) must **never** enter the child-visible RuntimeClosure
  (agent-bridle enforces the disjointness; this is the same fence as fixture #15 / the confined-bench
  `fs_read=All` residual, which must be closed for an honest bench).
- **Operator signatures for elevation** — the operator root Ed25519 key (`newt-core/ocap_store.rs`)
  is the principal that authorizes an `Elevation` (`widen_caveats`); a CID identifies the elevation
  record, it never authorizes it (L6). Key custody stays newt-side; bridle/mesh only verify.
- **Provenance lands in `solve_contract::contract_record`** next to the existing `agent_version` /
  `model_digest`: add the semantic `GrantId`, bridle version, and evidence CID per run — minimal,
  additive.
- **Integration uses the sanctioned candidate-SHA seam** (the `docs/testing/…/b1-store-bench`
  sibling-workspace `[patch.crates-io]` pattern the `no_git_pinned_bridle` guard exempts) — wire
  newt to a candidate agent-bridle/agent-mesh **without** forcing a release.

## Consequences

- **`feat/psyche` (#1492) delivers HONEST parity, not fake parity.** The path to a truthful
  OCAP-on table is: land the agent-bridle fidelity fix (bounded on, no silent widening) → grant
  task-needed authority via the **explicit RuntimeClosure** (loader paths, sh, etc. — declared,
  minimal, harness-disjoint) → run the live off/on bench → publish the README table. Parity is
  earned via explicit authorized closure, **never** via the silent-widening bug.
- OCAP-on may legitimately need the explicit closure to keep task performance up; that is the
  correct, auditable mechanism (distinct from over-granting).
- The Caveat attenuation lattice already partly formalized in `newt-agent/formal/CaveatLattice`
  (Lean) is the home for the L3/L6 attenuation proofs. Never commit an unchecked spec.

---
Model: Claude Opus 4.8 (1M context) | Harness: Claude Code | Operator: Shawn Hartsock | Time: 15:09 EDT | Date: 2026-08-09
