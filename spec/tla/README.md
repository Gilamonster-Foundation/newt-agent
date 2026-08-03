# `spec/tla/` — the TLA+ layer (temporal state-machine models)

The temporal proof machinery of the behavioral constitution (epic #1529). TLA+
answers *"across all orderings and failures, what states may the system reach?"* —
the agent-turn lifecycle, validate-before-execute ordering, retry / cancellation /
compaction / recovery, and exactly-once tool-result correlation.

**Status.** The reproducible, checksum-verified TLC harness, a *checked* smoke
spec, and the **first real checked model** — `ChatRecovery.tla` (the chat-loop
dispatch-recovery temporal contract for #1533 / epic #1529). The effective round
cap is a value CHOSEN once per behavior from `RoundCaps = {1, 2}`, so a SINGLE
`check.sh` run explores both the one-round crucible and the multi-round case —
the one-round check is a real merge gate, with no second `.cfg` to drift. It
genuinely rejects the historic bug: making a recovery advance the round violates
`RetryObligationIsSameRound`. Its seven invariants back the `BHV-ROUND-*` /
`BHV-RECOVERY-*` contracts in `../behavior-map.toml`.

The remaining models (`AgentTurn.tla`, `ContextRecovery.tla`) are **not** committed
yet — per the standing rule, an unchecked spec committed "to claim progress" is
worse than none. They land at steps 3 + 5 of the canonical implementation order
in [`../README.md`](../README.md) — after the `BehaviorEvent` alphabet (step 2)
exists to validate implementation traces against.

**`.cfg` note:** a liveness `PROPERTY` (e.g. `ChatRecovery`'s `Termination`)
requires the fairness-carrying `SPECIFICATION Spec`, not a bare `INIT`/`NEXT` — a
bare next-state relation omits `WF_vars(Next)` and TLC reports a false liveness
violation. Intended terminal states carry a stuttering action so the deadlock
check flags only genuine stuck states.

## Pinned toolchain (what this PR pins AND checks)

Only **TLC** (from `tla2tools`) is currently installed, pinned by checksum, and
executed by CI here:

| tool | version | pin |
|---|---|---|
| `tla2tools` (TLC / SANY / PlusCal) | **1.7.4** (TLC 2.19) | sha256 `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88` |

`check.sh` is fail-closed: it resolves the jar in order — `$TLA2TOOLS_JAR` (a
wrong version is a HARD error, never run) → `~/opt/tla2tools/` → cache → an
atomic, checksum-verified download — so a laptop with a local install and a clean
CI runner behave identically, a tampered jar is refused, and a missing/skipped
spec never yields success. Bump the version and the checksum in lock-step. Regression
tests: `spec/tla/test-check.sh`.

## Run

```bash
spec/tla/check.sh            # TLC-check every complete <Name>.tla + <Name>.cfg pair
spec/tla/check.sh Smoke      # just one; prints `tla-checked-count=N`
spec/tla/test-check.sh       # fail-closed regression tests for the runner
```

Expected on `Smoke`: *"Model checking completed. No error has been found."*

## Planned compatibility target: Apalache

Apalache (symbolic / inductive checking) is a **planned** second checker — it is
**not pinned or executed by this PR**. When it is adopted it needs a pinned
release artifact or immutable container digest plus its own checked smoke, added
in the same style as TLC above.

**Gotcha to carry forward:** Apalache requires a `\* @type: …;` annotation on
every `VARIABLE` / `CONSTANT` (e.g. `VARIABLE \* @type: Int; x`); TLC does not.
The real models will carry the annotations so both checkers accept them. (Same
requirement applies to pointing Apalache at the kyln `formal/` TLA+ specs.)

## What lives here vs. Lean

- **TLA+ (here)** — temporal / ordering / failure-combination properties:
  `NoEffectsBeforeValidation`, `ConfigFailureIsTerminal`, `CeilingNeverIncreases`,
  `RetriesBounded`, `AtMostOneOutputPerCall`, plus liveness.
- **Lean (`../../formal/NewtPolicy`)** — the pure per-step decisions (backend
  selection, decode classification, batch validation, budget arithmetic).

See `../behavior-map.toml` for which `BHV-*` contract each invariant discharges.
