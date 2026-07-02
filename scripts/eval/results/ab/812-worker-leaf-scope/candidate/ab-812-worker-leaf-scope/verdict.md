LIFT — lever 812-worker-leaf-scope shows a significant lift on exactly one cell (T2-humanize-duration crew qwen2.5-coder:32b, 5/5 vs 1/5, p=0.0238); all other cells are NO-LIFT or UNDERPOWERED.

Baseline arm: scripts/eval/results/sweeps/2026-07-01-pr802-baseline (sha 30b2120)
Candidate arm: scripts/eval/results/ab/812-worker-leaf-scope/candidate (sha 4fb6814)

## Verdict

**LIFT** — overall verdict for lever 812-worker-leaf-scope.

Of the 10 graded cells, 1 shows a statistically significant lift at n=5/arm (T2-humanize-duration crew qwen2.5-coder:32b: 5/5 candidate vs 1/5 baseline, p=0.0238 one-sided Fisher exact). The remaining 9 cells are NO-LIFT (candidate and baseline indistinguishable, p > 0.05) or UNDERPOWERED (directionally positive but not resolvable at this sample size). No UNGRADEABLE cells were observed in this run, so no hidden `grade_spec.rs` gap blocks this lift claim.

## Per-cell table

| cell | verdict | candidate | baseline | p (one-sided) | min n/arm for power |
|---|---|---|---|---|---|
| T2-humanize-duration crew qwen2.5-coder:32b | **LIFT** | 5/5 | 1/5 | 0.0238 | - |
| T2-humanize-duration crew qwen3-coder:30b | **UNDERPOWERED** | 4/5 | 3/5 | 0.5 | 32 |
| 010-decompose-god-function crew deepseek-r1:70b | **NO-LIFT** | 0/5 | 0/5 | 1 | - |
| 010-decompose-god-function crew devstral-small-2:24b | **NO-LIFT** | 1/5 | 1/5 | 0.7778 | - |
| 010-decompose-god-function crew qwen2.5-coder:14b | **NO-LIFT** | 5/5 | 5/5 | 1 | - |
| 010-decompose-god-function crew qwen2.5-coder:32b | **NO-LIFT** | 3/5 | 3/5 | 0.7381 | - |
| 010-decompose-god-function crew qwen3-coder:30b | **NO-LIFT** | 4/5 | 5/5 | 1 | - |
| T2-humanize-duration crew deepseek-r1:70b | **NO-LIFT** | 5/5 | 5/5 | 1 | - |
| T2-humanize-duration crew devstral-small-2:24b | **NO-LIFT** | 5/5 | 5/5 | 1 | - |
| T2-humanize-duration crew qwen2.5-coder:14b | **NO-LIFT** | 5/5 | 5/5 | 1 | - |

The T2-humanize-duration crew qwen3-coder:30b cell is UNDERPOWERED: the observed rates (4/5 vs 3/5) would need roughly **32** trials per arm to resolve at alpha=0.05.

## Expected-flip scorecard

| expected cell | observed verdict |
|---|---|
| 010-decompose-god-function crew devstral-small-2:24b | NO-LIFT |
| 010-decompose-god-function crew qwen2.5-coder:32b | NO-LIFT |
| 010-decompose-god-function crew deepseek-r1:70b | NO-LIFT |
| T2-humanize-duration crew qwen3-coder:30b | UNDERPOWERED |
| T2-humanize-duration crew qwen2.5-coder:32b | LIFT |

Of 5 expected-flip cells, only 1 (T2-humanize-duration crew qwen2.5-coder:32b) actually flipped to LIFT; the 3 expected 010-decompose-god-function flips did not materialize (all NO-LIFT), and the T2-humanize-duration qwen3-coder:30b expected flip landed UNDERPOWERED rather than resolved.

## Method

Fisher exact test, one-sided, alpha = 0.05. PASS-only scoring on ungameable rungs. n = 5 trials per arm per cell. p-values and pass counts above are taken verbatim from the source per-cell results — none recomputed here.

## Caveats

- **No UNGRADEABLE cells in this run.** All 10 cells produced a scoreable PASS/FAIL rate from the existing rubric; no hidden `grade_spec.rs` gap was hit here, so no lift claim in this verdict is blocked on `/grade-spec-author`. (If a future rerun of this lever surfaces UNGRADEABLE cells, that gate applies before any lift claim from those cells.)
- **Arms were run sequentially, not interleaved.** The baseline arm (sha 30b2120) and candidate arm (sha 4fb6814) were executed as separate sweeps rather than interleaved trial-by-trial. Endpoint drift (model/server state, load, quantization or version changes on the inference endpoints between runs) is **uncontrolled** and could inflate or deflate the observed differences, including the single significant LIFT cell.
- **Power floor at n=5/arm.** With 5 trials per arm, one-sided Fisher exact at alpha=0.05 is only crossed by an outcome pattern of roughly 5/5 vs ≤1/5 (or the symmetric reverse). This is a coarse floor: most real effect sizes at this n will land as NO-LIFT or UNDERPOWERED even if a true effect exists. The single LIFT cell here (5/5 vs 1/5) sits right at that threshold — treat it as a signal worth a confirmatory rerun, not a settled result, especially given the sequential-arms caveat above.
- **3 of 5 expected-flip cells did not flip.** The lever's hypothesis predicted lift on 010-decompose-god-function across three models; none showed movement (2 cells tied 5/5 vs 5/5 or near-identical, 1 tied 0/5 vs 0/5). This narrows the plausible mechanism of the lever to the T2-humanize-duration task rather than a general crew-leaf-scoping effect.
