# `spec/tla/` — the TLA+ layer (temporal state-machine models)

The temporal proof machinery of the behavioral constitution (epic #1529). TLA+
answers *"across all orderings and failures, what states may the system reach?"* —
the agent-turn lifecycle, validate-before-execute ordering, retry / cancellation /
compaction / recovery, and exactly-once tool-result correlation.

**Status: harness only.** This directory currently contains the reproducible,
checksum-verified toolchain harness and a *checked* smoke spec that proves it
works in CI. The real models (`AgentTurn.tla`, `ContextRecovery.tla`) are **not**
committed yet — per the standing rule, an unchecked spec committed "to claim
progress" is worse than none. They land at step 6, after the `BehaviorEvent`
alphabet (step 4) exists to validate implementation traces against.

## Pinned toolchain

| tool | version | pin |
|---|---|---|
| `tla2tools` (TLC / SANY / PlusCal) | **1.7.4** (TLC 2.19) | sha256 `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88` |
| Apalache (symbolic / inductive) | **0.58.3** | runs on OpenJDK 17 |

`check.sh` resolves the jar in order — `$TLA2TOOLS_JAR` → `~/opt/tla2tools/` →
cache → **download the pinned release and verify its sha256** — so a laptop with
a local install and a clean CI runner behave identically, and a tampered jar is
refused. Bump the version and the checksum in lock-step.

## Run

```bash
spec/tla/check.sh            # TLC-check every <Name>.tla that has a <Name>.cfg
spec/tla/check.sh Smoke      # just one
```

Expected on `Smoke`: *"Model checking completed. No error has been found."*

## Apalache note (remember this)

Apalache requires a `\* @type: …;` annotation on every `VARIABLE` (and on
`CONSTANT`s), e.g. `VARIABLE \* @type: Int; x`. TLC does not need it. When the
real models land they will carry the annotations so both `tlc` and `apalache-mc`
can check them. (Same gotcha applies to pointing Apalache at the kyln `formal/`
TLA+ specs.)

## What lives here vs. Lean

- **TLA+ (here)** — temporal / ordering / failure-combination properties:
  `NoEffectsBeforeValidation`, `ConfigFailureIsTerminal`, `CeilingNeverIncreases`,
  `RetriesBounded`, `AtMostOneOutputPerCall`, plus liveness.
- **Lean (`../../formal/NewtPolicy`)** — the pure per-step decisions (backend
  selection, decode classification, batch validation, budget arithmetic).

See `../behavior-map.toml` for which `BHV-*` contract each invariant discharges.
