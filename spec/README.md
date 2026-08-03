# `spec/` — newt's behavioral constitution (epic #1529)

The four-round #1526 review showed every blocker lived in the same policy seams:
backend selection, Responses decoding, validate-before-execute ordering,
tool-result correlation, and context budgeting. This tree makes those seams a
**constitution** — models that admit the correct behaviors plus a conformance
harness — so the implementation stays free to evolve but must keep producing
behaviors the constitution admits.

```
      TLA+                         Lean
 state-machine design       pure policy proofs
        \                       /
         \                     /
        executable behavioral oracle
                   |
            Rust conformance tests
                   |
             production code
```

- **`behavior-map.toml`** — the `BHV-*` contract registry. Every load-bearing
  rule has a durable id and a **per-layer** status (`lean` / `rust` / `tla` /
  `trace` + an overall `conformance`), so "modeled" is never conflated with
  "proved against production". A contract is `conformance = full` only when a
  checked model is differentially established to *refine* the code (an executable
  oracle) — today every row is `partial`.
- **`lint-behavior-map.py`** — the registry-reference linter: fails if a named
  Lean theorem / production path / Rust test is renamed out from under an entry.
  Strict on `lean` here; `--strict` (on `main`, post-merge) makes production/rust
  refs strict too. Wire into CI as `behavior-fast`.
- **`../formal/NewtPolicy/`** — the Lean layer (pure policy, machine-checked with
  `lake build`, no Mathlib). Shipped now: the backend-selection metamorphic
  theorems (`lean = proven` — the exact #1526 regression: adding/reordering
  unrelated backends cannot change an explicit selection), and the tool-batch
  **capability-token spec** (`lean = spec` — a validated call/batch carries its
  non-empty-id / non-empty-name / distinct-ids invariants by construction, and
  execution projects from the token, never from raw calls). Still to prove
  (`spec → proven`): a `validateBatch : RawBatch → Except _ ValidatedBatch` and
  its soundness, and a real `IsObject` predicate for `args_object` (today a
  `True` placeholder) — landing with the two-stage `CorrelatedBatch` /
  `ValidatedBatch` capability types (#1529 §3).
- **`tla/`** — the TLA+ layer (turn lifecycle, recovery). *Planned* — requires
  `tla2tools`/Apalache, not yet on this host; models are not committed until they
  can be TLC-checked (no ceremonial specs).

## What is NOT formalized

Exact JSON field placement, HTTP header casing, serde details, ANSI/ConPTY byte
behavior, tokenizer precision, log formatting — those get corpus, contract, and
differential tests, not proofs.

## Status

Seed. See #1529 for the full plan and priority order. Currently: the registry +
the first checked Lean proofs. Next: extract `newt-policy` capability types, then
`AgentTurn.tla`, then `BehaviorEvent` traces + `proptest-state-machine`.
