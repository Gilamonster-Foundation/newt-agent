# The harness, in-loop, on live Nemotron: fabrication eliminated; grounding is model-bound

**Date:** 2026-06-16 · **Model:** `nemotron-3-nano:4b` (local Ollama) · **Corpus:** 4 PyO3
crates from this repo (~12k tok, *fits-window* control) · **Measured by:** `newt-eval
score` (the verify oracle, #339/#340) · **Builds on:**
[`2026-06-14-fabrication-is-sampling-not-information-loss.md`](2026-06-14-fabrication-is-sampling-not-information-loss.md),
[`2026-06-15-retry-and-the-honest-gate.md`](2026-06-15-retry-and-the-honest-gate.md)

## The result

The first end-to-end test of the **in-loop** model-support-kit techniques
(`knowledge_base` + `verify_gate` + `retry`, all merged) against the live model the
incident came from — baseline vs. the `nemotron` profile, same model, same task, same
corpus:

| run | score | what shipped |
|---|---|---|
| **baseline** (no profile) | **0.0 — FAIL** | `examples/newt_core_example.py:6` → `from newt_core import get_greeting` |
| **profile** (`knowledge_base`+`verify_gate`+`retry`) | **1.0 — PASS** | no fabricated imports |

The baseline reproduces the incident exactly: the model invents `newt_core` from the
crate name (the cross-family confabulation) and calls a fabricated `get_greeting`.

The loop fired end-to-end (from the session log):

```
▸ profile 'nemotron' — knowledge_base, verify_gate, retry
INFO knowledge_base: FFI import surface injected crates=4
 turn 1 → model writes 4 fabricating files → ↩ retry: reverted 4 → ↻ re-prompt (1 left)
 turn 2 → fabricates again        → ↩ retry: reverted 4 → ↻ re-prompt (0 left)
 turn 3 → output passes the gate (no fabrication) → kept
```

`knowledge_base` injected the authoritative surface; `verify_gate` caught the
fabrication; `retry` reverted the flagged files (twice) and re-prompted; the loop
**converged to a non-fabricating output within the cap**.

## The honest two-part finding

A technique ships with its failure mode, not a verdict. Both halves of this are true
and both matter:

1. **Fabrication eliminated (gate-as-spec satisfied).** The confident hallucination the
   baseline shipped (`import newt_core`, 0.0) does not survive: the harness reverted it
   and the shipped output carries no fabricated import (1.0). newt does not ship the
   made-up API.

2. **The 4b *avoided* rather than *grounded* (the bounded caveat).** The final
   `newt_core.py` is import-free — literally `print('Hello, newt-core!')`. Under
   re-prompt pressure, `nemotron-3-nano:4b` retreated to a trivial no-import example
   instead of grounding to the real `newt_agent.core` the surface handed it. This is
   the **"honest no-output"** outcome the hardened-gate re-measurement predicted
   ("2/3 grounded, 1/3 honest no-output", #357): a weak model *avoids* the bad import
   rather than *correcting* to the right one. Grounding-vs-avoidance is **bounded by
   model capability** — a stronger model (the 33b, or qwen) is more likely to ground.

So: the harness converted *confident fabrication* → *honest non-fabrication* on a
fabrication-prone model — its job — and the residual *avoidance* is the model's limit,
not the harness's, and is exactly what the per-family-profile program exists to
measure. The score (0.0 → 1.0) tells the gate's story; the file contents
(`print(...)`) tell the capability story. Read both.

## Why this validates the kit

This is the model-support-kit thesis on live Nemotron: bring a fabrication-prone model,
the kit supplies the parts its failure mode needs (`knowledge_base` to hand it the
surface, `verify_gate`+`retry` to refuse the fabrication), and the result is honest
output where the baseline shipped a lie — *measured, per-family, by the rig*, not
asserted.

## Method (reproducible)

```bash
# corpus + surface (4 crates = fits-window, isolates the technique from the overflow confound)
pack_pyo3_corpus.sh --repo <newt-agent> --out $RIG --crates 4

# baseline vs profile, same model/task/corpus, sandboxed HOME, env -i
#   profile run seeds [profiles.nemotron] techniques=[knowledge_base,verify_gate,retry]
#   and threads NEWT_PROFILE=nemotron through the session
newt --no-splash code $WS              # baseline → fabricates newt_core (0.0)
NEWT_PROFILE=nemotron newt --no-splash code $WS   # profile → no fabrication (1.0)

newt-eval score --workspace $WS --surface-dir $RIG --json   # the verdict
```

Single-trial (the fabrication is stochastic — see the sampling-variance finding); a
`--repeats` sweep + a stronger-model arm (to exhibit the *grounding* outcome) is the
natural next measurement.

## Caveats

- One model, one trial, one corpus size. The point is the **in-loop wiring works on
  live Nemotron**, not a statistical claim — the sweep quantifies the lift.
- `nemotron-3-nano:4b` is a weak 4b; its *avoidance* is the expected low-capability tail,
  not a harness defect. Re-running on a stronger nemotron / qwen is the contrast that
  would show grounding (and is what the model × profile matrix is for).

Refs #80, #357, #73, #74, #368, #372, #375, #385.
