# Design laws

The invariants. Each links to the decision record that argues it. Moved here
from the README so the README stays a pointer, not a contract.

- **Local-first inference.** The default binary speaks only to local
  backends. Cloud providers are opt-in subprocess plugins speaking the
  JSON-RPC schema in [`plugins-protocol/`](../plugins-protocol/) — the opt-in
  is enforced at the **build** level, not a runtime flag.
- **Fail-closed OCAP.** Authority is a caveat lattice, not a denylist; a
  fixed safety floor no mode or grant can unlock. See
  [`decisions/agentic_object_capability_security.md`](./decisions/agentic_object_capability_security.md)
  and [`decisions/ocap_confinement_model.md`](./decisions/ocap_confinement_model.md).
- **Small crates, zero warnings, coverage-gated.** `just check` mirrors CI;
  the pre-push hook runs it. One operator's leverage *is* this discipline.
- **Patch, not prose.** Delegated work is verified by the harness (real
  diffs, real test runs — [`newt-eval/`](../newt-eval/)), never by trusting a
  model's summary of itself. The bench ratchet is the same law at release
  scale: verify by artifact, never by self-report.
- **Skills are on-demand context.** The prompt carries an index; bodies load
  when used. See [`decisions/agent-skills.md`](./decisions/agent-skills.md)
  and the bundled skills in [`.newt/bundled-skills/`](../.newt/bundled-skills/).
- **Issues are ground truth.** [`ROADMAP.md`](../ROADMAP.md) sequences
  delivery, but GitHub issue state is authoritative — the document is only
  the map.
- **Causal ordering, not wall-clock.** Timestamps are display *claims*; the
  conversation store orders on signed per-writer ticks + content hashes. See
  [`decisions/conversation_context_architecture.md`](./decisions/conversation_context_architecture.md).
