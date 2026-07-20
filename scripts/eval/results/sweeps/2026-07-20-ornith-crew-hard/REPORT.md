# Sweep Analysis: 2026-07-20-ornith-crew-hard

- **Sweep dir:** `scripts/eval/results/sweeps/2026-07-20-ornith-crew-hard`
- **Git SHA:** `8844352` (main + the #1321 config bridge; merged as `35a2536`)
- **Graded rows:** 25 (5 tasks × crew × Ornith-1.0-35B-NVFP4 × n=5)
- **Status:** DONE=true; errors.log empty; mean trial 25s
- **Context:** the companion to `2026-07-20-ornith-crew-ladder` (T0–T3 went
  20/20 with `leaves=1` everywhere — the ladder never engaged decomposition).
  This sweep runs the decomposition-demanding rungs to find the boundary.

## Summary

**The competence boundary is found, and it is exactly the E/#672 failure mode.**
The three rungs a strong model can carry in one leaf are clean 5/5
(008-extract-helper, 011-state-machine-drain, 013-generic-bounds). The two rungs
where *decomposition itself is the task* both drop to **3/5**:
010-decompose-god-function and 014-multi-file-extract.

**The failure shape is the signature, not noise.** Of the 4 FAILs, **3 are
`leaves=1` with `plan_rc=0`** — the crew UNDER-decomposed (one leaf "did work"
on the right file), *believed it succeeded*, and the behavioral grader failed
the artifact: the silent-bad-output signature the pr802 autopsy documented
("every inspected FAIL log ended `✓ plan complete`"). Only 1 FAIL was a genuine
multi-leaf attempt that errored (`leaves=3, plan_rc=1`).

**The headline: the boundary did not move with a stronger model.** Ornith-35B on
010/crew lands at 3/5 (60%, Wilson [23%, 88%]) — statistically indistinguishable
from the pr802 baseline's pooled 14/25 (56%, [37%, 73%]) across five 14–70B
ollama models. A frontier-class local model plus the newly-landed navigation
stack (#1277) does **not** lift the decomposition rungs. **This re-confirms the
corpus thesis: the ceiling is the harness mechanism, not the model** — the
mechanism levers (per-leaf behavioral gate, worker grounding, planner grounding;
RATCHET.md §3) remain the path, and this sweep is their pre-lever baseline at
n≥5.

**OCAP note:** no access override on this path — all 25 trials ran under the
confined default engine with the `session ⊓ crew_clamp` dispatch bound. The 18/25
overall pass rate is utility-under-confinement, measured.

## Per-cell results

| task | mode | model | pass | fail | n | pass-rate | Wilson 95% |
|---|---|---|---|---|---|---|---|
| 008-extract-helper | crew | Ornith-1.0-35B-NVFP4 | 5 | 0 | 5 | 100% | [57%, 100%] |
| 010-decompose-god-function | crew | Ornith-1.0-35B-NVFP4 | 3 | 2 | 5 | 60% | [23%, 88%] |
| 011-state-machine-drain | crew | Ornith-1.0-35B-NVFP4 | 5 | 0 | 5 | 100% | [57%, 100%] |
| 013-generic-bounds | crew | Ornith-1.0-35B-NVFP4 | 5 | 0 | 5 | 100% | [57%, 100%] |
| 014-multi-file-extract | crew | Ornith-1.0-35B-NVFP4 | 3 | 2 | 5 | 60% | [23%, 88%] |

FAIL autopsy pointers (cells preserved under `/var/tmp/newt-sweeps/…`, KEEP=fail):

| task | leaves | plan_rc | shape |
|---|---|---|---|
| 010 ×2 | 1 | 0 | under-decomposed, believed success (silent bad output) |
| 014 ×1 | 3 | 1 | genuine multi-leaf attempt, errored |
| 014 ×1 | 1 | 0 | under-decomposed, believed success |

## Follow-ups this data funds

1. **The RATCHET.md levers, in order:** (a) per-leaf behavioral gate (an
   under-decomposed leaf must not report success), (b) planner grounding (the
   plan should *know* 010/014 need >1 leaf). This baseline is what they'll be
   measured against.
2. **Single-mode arm** still blocked on the worker's ollama-only env-shim vs
   vLLM/openai (recorded in the ladder REPORT).
3. `newt plan` writes `plan.run.toml` into the *invoking* cwd, not `--dir` — a
   litter wart observed during both sweeps.

## Security invariant

Model identities only; endpoints live in the operator's local template
(RATCHET.md invariant).
