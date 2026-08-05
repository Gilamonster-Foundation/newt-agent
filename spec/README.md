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
- **`lint-behavior-map.py`** — the EXACT, fail-closed registry-reference linter.
  References are structured (`{module, symbol}` for Lean; `{path, symbol}` for
  Rust/production; `{spec, invariant}` for TLA) and validated to their intended
  artifact — a renamed theorem, test, or production symbol fails, as do
  zero/ambiguous/duplicate refs, invalid status, a missing required field, and an
  unmet `conformance = "full"`. A `rust_tests` ref must resolve to a fn carrying a
  recognized **test attribute** (`#[test]`, `#[tokio::test]`, …) — deleting the
  attribute orphans the ref even though the fn remains. A `tla` ref must name an
  invariant that is both **defined** as an operator in `<spec>.tla` and
  **declared** in an `INVARIANT` line of `<spec>.cfg`. Lean refs are resolved
  against the **whole `formal/` layer** — every lake lib, not just `NewtPolicy/` —
  by namespace-qualified decl name (`.lake` build copies are excluded so they
  cannot fabricate ambiguity); this is what lets the registry cite the #1528
  kernels below. Fail-closed: an unresolved reference is an ERROR unless it carries
  an explicit `pending_pr = <n>` (its artifact lives on an unmerged PR — the psyche
  seed is stacked on #1526), in which case it WARNS, naming the PR. `--strict` fails
  even those; run it on `main` after #1526 merges, then delete the markers.
  Self-tests: `test-lint-behavior-map.py` (incl. the required negatives + the
  broadened-scope / `.lake`-exclusion / genuine-ambiguity cases). The linter is pure
  Python (no Lean/JDK),
  so CI runs it on **every Rust-source change** — `behavior-registry.yml` (paths:
  `spec/**`, `formal/**`, `**/*.rs`, Cargo manifests) — precisely so a Rust-only PR
  cannot silently orphan a reference. The expensive Lean+TLC checks run only on
  `spec/`/`formal/` changes via `behavior-formal.yml`.
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
  `ValidatedBatch` capability types (#1529 §3). The **#1528 Responses long-turn
  kernels** are additional lake libs under `../formal/` (`lean = proven`, namespace
  `NewtPolicy.{CompactionProvenance, CompactionSpill, ResponsesUsage, ResponsesWire,
  CompactionLifecycle}`), registered as `BHV-{PROVENANCE,SPILL,USAGE,WIRE}-*` and
  `BHV-CONTEXT-004` — the first contracts whose artifacts are landed (no
  `pending_pr`). `CompactionLifecycle.no_lifecycle_step_advances_round` is the Lean
  counterpart of the reconciliation's combined round machine: compaction never
  advances the logical round.
- **`tla/`** — the TLA+ layer (turn lifecycle, recovery). **Harness only**: the
  fail-closed, checksum-pinned `tla2tools` (1.7.4) TLC runner (`check.sh`) + a
  *checked* smoke spec prove the toolchain works in CI. Only **TLC** is pinned and
  executed; **Apalache is a planned compatibility target**, not pinned/run here
  (see `tla/README.md`). The real models (`AgentTurn.tla`, `ContextRecovery.tla`)
  are NOT committed yet — no ceremonial specs.

## What is NOT formalized

Exact JSON field placement, HTTP header casing, serde details, ANSI/ConPTY byte
behavior, tokenizer precision, log formatting — those get corpus, contract, and
differential tests, not proofs.

## Implementation order (canonical — do not drift)

The observation surface must exist *before* the temporal model, so the model
consumes a deliberate `BehaviorEvent` alphabet rather than inventing its own:

1. Rust `CorrelatedBatch` / `ValidatedBatch` capability types (the compile-time
   choke point: `RawBatch → CorrelatedBatch → ValidatedBatch → execute_batch`).
2. Redacted `BehaviorEvent` observation alphabet (ids / counts / classifications
   only — never prompts, source, arguments, reasoning, or secrets).
3. Checked `AgentTurn.tla` model (consuming that alphabet).
4. Rust trace projection + TLA+ / Apalache-ITF implementation-trace validation.
5. `ContextRecovery.tla` + deeper state-machine / property testing.

The TLC tooling bootstrap (this PR) is landed now; the models above are gated on
steps 1–2. See #1529 for the full plan.
