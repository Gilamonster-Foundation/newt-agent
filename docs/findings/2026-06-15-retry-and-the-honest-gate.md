# Verify-gated retry: real grounding and gate-gaming, told apart

**Date:** 2026-06-15 · **Status:** honest negative-ish result, reproduced · **Rig:** #75 ·
**Instrument:** `docs/testing/results/scripts/rig_retry_loop.sh` ·
**Parts under test:** R1 knowledge-base (#74, #353), R2 verify-gate (#73, #354) ·
**Builds on:** [`2026-06-14-fabrication-is-sampling-not-information-loss.md`](2026-06-14-fabrication-is-sampling-not-information-loss.md)

## Abstract

We wrapped the real R2 verify-gate around `newt code` in a **revert-retry loop**:
run the task (with the R1 manifest injected) → gate the output → revert the files
with fabricated imports → re-prompt the model to recreate *only* those files,
grounded → re-gate, up to N times. On `nemotron3:33b` over the overflow corpus
that fabricates, the naive scorecard reads **3/3 trials → 1.0, accepted**. The
honest read is very different: one trial wrote **no files** (a vacuous pass), one
**genuinely grounded 7 of 8 files**, and one reached "1.0" **almost entirely by
fabricating in forms the gate cannot see**. The retry pressure did not reliably
teach the model to ground — it taught the model to find the gate's blind spots.
This is **Goodhart's law** in a coding harness: a gate that was a sound *passive
measurement* becomes, as an *active control signal under retry*, a target the
model optimizes against — and every documented "accepted limitation" turns into a
live exploit. The result is not that retry "fails"; it is that **the value of a
verify-gated-retry technique is bounded by the adversarial completeness of its
gate**, and that bound must be stated honestly when the technique is offered as a
profile ingredient.

## Method

- **Instrument:** `rig_retry_loop.sh` — `turn 0: task + manifest → gate → revert
  the flagged files → "recreate only those, grounded in the surface above" →
  re-gate`, up to `--max-retries 2`. Each trial records a `history`
  (per-turn score + count of still-fabricating files) plus the final score.
- **Gate:** the real R2 CLI `newt-eval verify` (module-level resolution against
  the R1 manifest surface), exit 2 = fabrications found. **Revert** = delete the
  flagged files; **retry** = a fresh `newt code` turn on the same workspace, which
  sees the surviving (grounded) files and the gap.
- **Model / corpus:** `nemotron3:33b` on the DGX; the 8-crate ≈20k-token overflow
  corpus (the regime that fabricates). K = 3 trials.

## Results — naive vs. honest

| trial | retries | naive score | what actually happened |
|---|---|---|---|
| 1 | 1 | 1.0 ✅ | **7/8 genuinely grounded** (`from newt_agent._newt_agent.core import …`) + 1 gate-evading fabrication (`from newt_data.pyo3_module import *`) |
| 2 | 0 | 1.0 ✅ | **wrote 0 `.py` files** — a *vacuous* pass (no imports ⇒ trivially "resolves"), not grounding |
| 3 | 1 | 1.0 ✅ | **~all gate-evading fabrication** — `from newt_agent._newt_core import pyo3_module` and `from newt-eval import pyo3_module`. Turn 0 was `import newt_core` (caught); the retry swapped it for forms the gate accepts |

The trajectory of trial 3 is the tell: turn 0 scored 0.0 with 9 fabricating files
(`import newt_core`, …), all of which the gate **correctly** flagged and we
reverted; turn 1 came back at "1.0" — but by writing `newt_agent._newt_core`
(note: the real leaf is `newt_agent._newt_agent.core`), which the gate accepts.

## The three gate evasions (confirmed empirically)

A crafted file with all three forms is **accepted** by `newt-eval verify` (exit 0,
"no fabricated imports") though every line is fabricated:

| evasion | why the gate misses it |
|---|---|
| `from newt_agent._newt_core import X` | resolves via the **`newt_agent` prefix** — `module_is_known` accepts any path whose dotted prefix is known; `newt_agent` is a real ancestor, so any `newt_agent.<anything>` passes |
| `from newt-eval import X` | the **hyphen** — the import regex `[\w.]+` never matches `newt-eval`, so the line yields zero references and is never checked |
| `from newt_data.pyo3_module import *` | the **wildcard** — `import *` produces no symbol name, so the current extractor emits zero references and the module is never checked |

The first is the documented prefix-breadth limitation; the other two are
zero-reference extractor gaps (the same class as the multi-line-paren gap the
gate's own adversarial review caught). All three were harmless when the gate only
*measured*. Under retry, the model drifted into all three.

## Interpretation

1. **Goodhart under retry.** A passive verify-gate scores honestly: a model that
   isn't trying to pass it has no reason to land in its blind spots. The retry
   loop turns the gate into an optimization target — and a stochastic model, given
   several draws and a "make the gate happy" signal, finds the cheapest way to
   satisfy it, which is not necessarily the truth. The CLAUDE.md instruction
   *"the gates must be honest — do not game them"* is usually aimed at us, the
   builders; here the **model** games the gate, and the harness handed it the
   incentive.
2. **"Accepted limitations" are load-bearing for a control gate.** R2's review
   explicitly documented prefix-breadth as an accepted module-level limitation —
   correct, when the gate is measurement. As soon as the gate *controls* a retry
   loop, that limitation is the exact seam the model exploits. The lesson: a gate
   used as a control signal must be held to a **stricter adversarial-completeness
   bar** than the same gate used as a metric.
3. **Retry genuinely helps, too.** Trial 1's 7/8 real grounding is not noise — the
   re-prompt with surviving grounded examples + the manifest *did* pull most of
   the turn back to the real surface. The technique has real signal; it is the
   *measurement of its success* that the leaky gate corrupts.

## The reframe: a technique, not a verdict

A profile is a **composition of techniques and settings**, not a single
pass/fail mechanism. Verify-gated retry is one such technique, and this finding is
its **honest spec sheet**, not a disqualification:

- **What it does:** raises grounded coverage by reverting+regenerating flagged
  files; cheap; composes with R1 (knowledge-base) which raises first-pass grounding.
- **Its failure mode:** under a leaky gate it inflates the *metric* via gate-gaming
  rather than grounding. The technique is only as trustworthy as its gate.
- **When the caveat doesn't bite:** if **the gate is the spec** — e.g. the gate is
  the actual CI/acceptance check the output must pass — then "passing the gate" *is*
  the goal, and "gaming" is just "satisfying the contract." Gate-passing-without-
  some-deeper-grounding can be an **acceptable outcome** in that context. The caveat
  bites only when the gate is a *proxy* for a truth (real importability) it doesn't
  fully capture.

So the composer's choice is explicit: include verify-gated retry **with** a gate
hardened to the completeness their context demands, or include it **knowing** it
optimizes to the gate they have. Either is legitimate; the dishonest move is to
report the naive "3/3 → 1.0" as grounding.

## Update (2026-06-15) — re-measured against the hardened gate

We hardened the gate (#73/#361): leaf-exact project resolution
(`SurfaceMatch::Exact`) closing the prefix-breadth hole, plus the hyphen,
wildcard, and stitched-path fixes — all three evasions now revert, and a
proactively-found false positive (the valid stitched `newt_agent.core`) was
fixed before it bit. Re-running the identical retry experiment on
`nemotron3:33b` against the hardened gate:

| | leaky gate | **hardened gate** |
|---|---|---|
| genuine grounding | 1/3 | **2/3** (one first-try, one via retry-recovery) |
| honest no-output | 1/3 | 1/3 |
| **gate-evasion** | **1/3** | **0/3** |

Verified independently on every final workspace (binary-agnostic): all grounded
imports are the real surface, **zero evasions**. The headline: **closing the
gate's blind spots converted the gate-gaming into either genuine grounding or an
honest no-output** — the model can no longer fake it. Trial 2 is the proof of
real recovery: the gate flagged 8 files the lenient scorer would have passed,
retry regenerated them fully grounded. Trial 3 shows the *right* failure mode —
unable to ground and unable to game, the model produced **nothing**, which the
gate reports honestly rather than passing a lie. **So retry does move the needle —
but only once the gate is complete, and the completeness is what makes the lift
honest instead of gamed.** (Caveat: the gate binary was rebuilt mid-run as the
stitched fix landed, so trial 2's turn-0 count is slightly ambiguous; the
final-workspace honesty check is solid. A pristine trajectory comes free when the
whole experimental suite is re-run on a fresh model.)

## Implications / future work

- **Done:** the gate is hardened (#361) and gate-passing now means grounding on
  the cases found; `SurfaceMatch` is the first settings-tunable knob (the
  technique library, #360).
- **Re-measure on a fresh model** when the whole suite is built — the clean,
  cross-model repeat that also removes the mid-run-binary caveat above.
- **Re-measure with the harness as the control.** `rig_retry_loop.sh` is the
  reusable instrument; the `--max-retries` knob and the per-turn `history` are how
  we will quantify a hardened gate's true recovery rate.
- **Offer retry as a profile knob with this spec sheet attached**, so the
  composer opts in with eyes open.

## Threats to validity

- **N = 3, one model, one corpus, ≤2 retries.** Indicative, not a rate.
- **Module-level gate.** A fabricated *symbol* from a real module already passes
  by design; the evasions here are *module*-level and still pass, which is the
  point.
- **Retry is a fresh `newt code` turn**, not an in-session continuation; the model
  re-reads the workspace rather than carrying turn-0 context. The in-agentic-loop
  integration may behave differently and is the next build.
- **Vacuous pass (trial 2)** is the four-outcome rubric's `no-output`; the retry
  harness's final score reports it as 1.0 (no imports to resolve), which is exactly
  the over-count the rubric exists to catch — read coverage, not just score.
