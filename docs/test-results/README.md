# Test results — the model survey matrix

Per-release results of running the model survey ([methodology](../methodology/))
across every model we currently have, on each hardware target. Each result file
is a matrix of **model × hardware × outcome** for one newt-agent version.

The point is a *baseline we improve against*: 0.6.8 is where most families fail
under context overflow; each subsequent release should move cells from ❌/∅ → ✅,
starting with the **nemotron family** (the 0.6.9 target — see below).

## Index

| version | experiment | summary |
|---|---|---|
| [0.6.8](0.6.8-pyo3-examples.md) | PyO3-examples under context overflow (8-crate corpus) | baseline across all gpu-runner (4060 Ti) + DGX Spark models |

## Outcomes

✅ PASS · ❌ FAIL (fabricated imports) · ∅ no-output (wrote no `.py`) ·
⚠ timeout/error. See the [methodology](../methodology/#outcomes-every-cell-is-data)
— **every cell is data**, including the failures.

## The improvement line

- **0.6.8** — baseline. Establishes which model families the current harness
  already supports (qwen-coder) vs. fabricates under (nemotron).
- **0.6.9** — **make the nemotron family pass.** The cross-family finding
  ([findings/2026-06-14](../findings/2026-06-14-cross-family-confabulation.md))
  shows nemotron fabricates the API surface where qwen-coder resolves it; 0.6.9
  is the per-family support work (decomposition / curated context / verify gate /
  the "model-family pack") that moves the nemotron cells to ✅. We may not get
  *every* model passing — we iterate once the nemotron family is green.
